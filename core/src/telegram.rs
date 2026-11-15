use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::events::Db;

const TG_TEXT_LIMIT: usize = 4000;

pub struct Tg {
    http: reqwest::Client,
    base: String,
    allowed: Vec<i64>,
}

impl Tg {
    pub fn from_env() -> Result<Self> {
        let token =
            std::env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN is required")?;
        if token.trim().is_empty() {
            bail!("TELEGRAM_BOT_TOKEN is empty");
        }
        let allowed = std::env::var("TELEGRAM_ALLOWED_CHAT_IDS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .collect();
        // Long polling holds the request for 30s, so the client timeout must be longer.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()?;
        let api = std::env::var("TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".into());
        Ok(Self {
            http,
            base: format!("{}/bot{token}", api.trim_end_matches('/')),
            allowed,
        })
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

    /// The bot token is worth nothing without a chat allow-list; anything else
    /// that talks to it must be rejected here, before an event is created.
    async fn on_update(&self, u: &Value, db: &Db) -> Result<()> {
        let msg = &u["message"];
        let Some(chat_id) = msg["chat"]["id"].as_i64() else {
            return Ok(());
        };
        let Some(text) = msg["text"].as_str() else {
            return Ok(()); // photos, stickers: nothing to run
        };
        if !self.allowed.contains(&chat_id) {
            eprintln!("rejected chat {chat_id}: add it to TELEGRAM_ALLOWED_CHAT_IDS");
            self.send(
                chat_id,
                &format!("Доступ закрыт. Чтобы разрешить этот чат, добавь в .env:\nTELEGRAM_ALLOWED_CHAT_IDS={chat_id}"),
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
