use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::events::Db;

const TG_TEXT_LIMIT: usize = 4000;

pub struct Tg {
    http: reqwest::Client,
    base: String,
    allowed_chats: Vec<i64>,
    allowed_users: Vec<i64>,
}

fn id_list(name: &str) -> Vec<i64> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect()
}

impl Tg {
    pub fn from_env() -> Result<Self> {
        let token =
            std::env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN is required")?;
        if token.trim().is_empty() {
            bail!("TELEGRAM_BOT_TOKEN is empty");
        }
        // TELEGRAM_ALLOWED_CHAT_IDS is the old name; in a private chat it is the
        // same number as the user id, so old configs keep working.
        let allowed_chats = id_list("TELEGRAM_ALLOWED_CHAT_IDS");
        let allowed_users = id_list("TELEGRAM_ALLOWED_USER_IDS");
        // Long polling holds the request for 30s, so the client timeout must be longer.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()?;
        let api = std::env::var("TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".into());
        Ok(Self {
            http,
            base: format!("{}/bot{token}", api.trim_end_matches('/')),
            allowed_chats,
            allowed_users,
        })
    }

    /// Private chats only: in a group or channel the agent would be talking to
    /// strangers. Ownership is then checked per user.
    fn permitted(&self, kind: &str, chat_id: i64, user_id: i64) -> bool {
        kind == "private"
            && (self.allowed_chats.contains(&chat_id) || self.allowed_users.contains(&user_id))
    }


    pub async fn poll_forever(&self, db: Arc<Db>) {
        let mut offset: i64 = db
            .kv_get("tg_offset")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        loop {
            match self.get_updates(offset).await {
                Ok(updates) => {
                    for u in updates {
                        if let Some(id) = u["update_id"].as_i64() {
                            offset = offset.max(id + 1);
                        }
                        if let Err(e) = self.on_update(&u, &db).await {
                            eprintln!("telegram update: {e:#}");
                        }
                    }
                    if let Err(e) = db.kv_set("tg_offset", &offset.to_string()) {
                        eprintln!("saving tg offset: {e:#}");
                    }
                }
                Err(e) => {
                    eprintln!("telegram poll: {e:#}");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    }

    async fn get_updates(&self, offset: i64) -> Result<Vec<Value>> {
        let v: Value = self
            .http
            .post(format!("{}/getUpdates", self.base))
            .json(&json!({"offset": offset, "timeout": 30, "allowed_updates": ["message"]}))
            .send()
            .await?
            .json()
            .await?;
        if v["ok"] != true {
            bail!("getUpdates: {v}");
        }
        Ok(v["result"].as_array().cloned().unwrap_or_default())
    }

    /// The bot token is worth nothing without an allow-list; anything else that
    /// talks to it must be rejected here, before an event is created.
    async fn on_update(&self, u: &Value, db: &Db) -> Result<()> {
        let msg = &u["message"];
        let chat = &msg["chat"];
        let Some(chat_id) = chat["id"].as_i64() else {
            return Ok(());
        };
        let Some(text) = msg["text"].as_str() else {
            return Ok(()); // photos, stickers: nothing to run
        };
        let kind = chat["type"].as_str().unwrap_or("");
        let user_id = msg["from"]["id"].as_i64().unwrap_or(chat_id);
        if kind != "private" {
            eprintln!("ignoring {kind} chat {chat_id}: private chats only");
            return Ok(());
        }
        if !self.permitted(kind, chat_id, user_id) {
            eprintln!("rejected user {user_id}: add it to TELEGRAM_ALLOWED_USER_IDS");
            self.send(
                chat_id,
                &format!(
                    "Доступ закрыт. Чтобы разрешить, добавь в /opt/hermit/hermit.env:\nTELEGRAM_ALLOWED_USER_IDS={user_id}"
                ),
            )
            .await?;
            return Ok(());
        }
        let payload = json!({
            "chat_id": chat_id,
            "text": text,
            "message_id": msg["message_id"].as_i64(),
        });
        db.push_event("message", &payload.to_string())?;
        Ok(())
    }

    pub async fn send(&self, chat_id: i64, text: &str) -> Result<()> {
        for chunk in chunks(text, TG_TEXT_LIMIT) {
            let resp = self
                .http
                .post(format!("{}/sendMessage", self.base))
                .json(&json!({"chat_id": chat_id, "text": chunk, "disable_web_page_preview": true}))
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!(
                    "sendMessage HTTP {status}: {}",
                    body.chars().take(300).collect::<String>()
                );
            }
        }
        Ok(())
    }
}

/// Telegram caps a message at 4096 chars; split on newlines, hard-cut long lines.
pub fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let mut line: String = line.chars().take(max).collect();
        if line.is_empty() {
            line.push('\n');
            continue;
        }
        if !cur.is_empty() && cur.chars().count() + line.chars().count() + 1 > max {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(&line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push("(пусто)".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tg(chats: &[i64], users: &[i64]) -> Tg {
        Tg {
            http: reqwest::Client::new(),
            base: String::new(),
            allowed_chats: chats.to_vec(),
            allowed_users: users.to_vec(),
        }
    }

    #[test]
    fn only_allowed_users_in_private_chats() {
        let t = tg(&[], &[42]);
        assert!(t.permitted("private", 42, 42));
        assert!(!t.permitted("private", 7, 7));
        // A group is answered even if every member is allowed.
        assert!(!t.permitted("group", 42, 42));
        assert!(!t.permitted("channel", 42, 42));

        // Old chat-id config still works, and an empty one allows nobody.
        let old = tg(&[99], &[]);
        assert!(old.permitted("private", 99, 99));
        assert!(!tg(&[], &[]).permitted("private", 99, 99));
    }

    #[test]
    fn splits_long_text_without_losing_content() {
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let parts = chunks(&text, 100);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(
                p.chars().count() <= 100,
                "chunk too big: {}",
                p.chars().count()
            );
        }
        let joined: String = parts.concat();
        assert_eq!(joined.matches("line ").count(), 500);
    }
}
