use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
            .json(&json!({"offset": offset, "timeout": 30,
                          "allowed_updates": ["message", "callback_query"]}))
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
        if let Some(cq) = u.get("callback_query") {
            return self.on_callback(cq, db).await;
        }
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

    /// Approve/Deny button pressed. Same allow-list as messages: the click is
    /// checked before it can create an event.
    async fn on_callback(&self, cq: &Value, db: &Db) -> Result<()> {
        let chat = &cq["message"]["chat"];
        let Some(chat_id) = chat["id"].as_i64() else {
            return Ok(());
        };
        let kind = chat["type"].as_str().unwrap_or("");
        let user_id = cq["from"]["id"].as_i64().unwrap_or(0);
        if !self.permitted(kind, chat_id, user_id) {
            return Ok(());
        }
        let Some((approve, id)) = cq["data"].as_str().and_then(parse_callback) else {
            return Ok(());
        };
        db.push_event(
            "approval",
            &json!({"chat_id": chat_id, "id": id, "approve": approve}).to_string(),
        )?;
        self.answer_callback(
            cq["id"].as_str().unwrap_or(""),
            if approve {
                "Принято"
            } else {
                "Отклонено"
            },
        )
        .await
    }

    pub async fn send(&self, chat_id: i64, text: &str) -> Result<()> {
        for chunk in chunks(text, TG_TEXT_LIMIT) {
            self.send_formatted(chat_id, &chunk, None).await?;
        }
        Ok(())
    }

    /// The approval prompt: the keyboard goes on the last chunk.
    pub async fn send_approval(&self, chat_id: i64, text: &str, id: i64) -> Result<()> {
        let parts = chunks(text, TG_TEXT_LIMIT);
        let last = parts.len() - 1;
        for (i, chunk) in parts.iter().enumerate() {
            let markup = (i == last).then(|| {
                json!({"inline_keyboard": [[
                    {"text": "✅ Выполнить", "callback_data": format!("approve:{id}")},
                    {"text": "⛔ Отклонить", "callback_data": format!("deny:{id}")},
                ]]})
            });
            self.send_formatted(chat_id, chunk, markup).await?;
        }
        Ok(())
    }

    /// Markdown becomes Telegram HTML. If Telegram still refuses the markup,
    /// the same chunk goes as plain text: an odd marker must not lose an answer.
    async fn send_formatted(&self, chat_id: i64, chunk: &str, markup: Option<Value>) -> Result<()> {
        let html = crate::md::to_html(chunk);
        let mut plain =
            json!({"chat_id": chat_id, "text": chunk, "disable_web_page_preview": true});
        // Markup can push a chunk over Telegram's 4096-unit cap, in which case
        // plain text is the only form that fits.
        if html.encode_utf16().count() <= TG_TEXT_LIMIT {
            let mut rich = json!({"chat_id": chat_id, "text": html, "parse_mode": "HTML",
                                  "disable_web_page_preview": true});
            if let Some(m) = &markup {
                rich["reply_markup"] = m.clone();
            }
            match self.send_message(rich).await {
                Ok(()) => return Ok(()),
                // Only a 400 means the markup was refused; a timeout may have
                // delivered the message already, so it must not be resent.
                Err(e) if format!("{e:#}").contains("HTTP 400") => {
                    eprintln!("telegram html send: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(m) = markup {
            plain["reply_markup"] = m;
        }
        self.send_message(plain).await
    }

    /// One Bot API method. The error keeps Telegram's description, because
    /// "message is not modified" and "message is too long" need different
    /// handling.
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "{method} HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let json: Value = serde_json::from_str(&text).context("Telegram returned non-JSON")?;
        Ok(json["result"].clone())
    }

    async fn send_message(&self, body: Value) -> Result<()> {
        self.call("sendMessage", body).await?;
        Ok(())
    }

    async fn send_plain_id(&self, chat_id: i64, text: &str) -> Result<i64> {
        let result = self
            .call(
                "sendMessage",
                json!({"chat_id": chat_id, "text": text, "disable_web_page_preview": true}),
            )
            .await?;
        Ok(result["message_id"].as_i64().unwrap_or(0))
    }

    async fn edit_plain(&self, chat_id: i64, id: i64, text: &str) -> Result<()> {
        self.call(
            "editMessageText",
            json!({"chat_id": chat_id, "message_id": id, "text": text,
                   "disable_web_page_preview": true}),
        )
        .await?;
        Ok(())
    }

    async fn edit_formatted(&self, chat_id: i64, id: i64, chunk: &str) -> Result<()> {
        let html = crate::md::to_html(chunk);
        if html.encode_utf16().count() <= TG_TEXT_LIMIT {
            let body = json!({"chat_id": chat_id, "message_id": id, "text": html,
                              "parse_mode": "HTML", "disable_web_page_preview": true});
            match self.call("editMessageText", body).await {
                Ok(_) => return Ok(()),
                Err(e) if format!("{e:#}").contains("not modified") => return Ok(()),
                Err(e) if format!("{e:#}").contains("HTTP 400") => {
                    eprintln!("telegram html edit: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }
        self.edit_plain(chat_id, id, chunk).await
    }

    /// Telegram clears "typing" after five seconds; four seconds keeps it on
    /// through a long model call without hammering the API.
    pub fn typing_loop(self: &Arc<Self>, chat_id: i64) -> tokio::task::JoinHandle<()> {
        let tg = self.clone();
        tokio::spawn(async move {
            loop {
                let _ = tg
                    .call(
                        "sendChatAction",
                        json!({"chat_id": chat_id, "action": "typing"}),
                    )
                    .await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        })
    }

    async fn answer_callback(&self, id: &str, text: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/answerCallbackQuery", self.base))
            .json(&json!({"callback_query_id": id, "text": text}))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "answerCallbackQuery HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }
}

/// `approve:12` / `deny:12` → verdict and id.
fn parse_callback(data: &str) -> Option<(bool, i64)> {
    let (verdict, id) = data.split_once(':')?;
    let approve = match verdict {
        "approve" => true,
        "deny" => false,
        _ => return None,
    };
    Some((approve, id.parse().ok()?))
}

/// Telegram caps a message at 4096 chars; split on newlines, hard-cut long lines.
pub struct Streamer {
    tg: Arc<Tg>,
    chat_id: i64,
    state: Arc<Mutex<State>>,
}

struct State {
    text: String,
    message_id: Option<i64>,
    last: Instant,
    pending: bool,
}

impl Streamer {
    pub fn new(tg: Arc<Tg>, chat_id: i64) -> Self {
        Self {
            tg,
            chat_id,
            state: Arc::new(Mutex::new(State {
                text: String::new(),
                message_id: None,
                last: Instant::now(),
                pending: false,
            })),
        }
    }

    /// True once anything was shown, so a failed stream cannot be retried
    /// silently and double-posted.
    pub fn started(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.message_id.is_some() || !st.text.is_empty()
    }

    fn delta(&self, chunk: &str) {
        let (snapshot, id, chat_id) = {
            let mut st = self.state.lock().unwrap();
            st.text.push_str(chunk);
            let len = st.text.chars().count();
            if st.pending || len == 0 || len > TG_TEXT_LIMIT - 500 {
                return;
            }
            if st.message_id.is_none() && len < 20 {
                return;
            }
            // One edit per two seconds: Telegram rate-limits edits per chat.
            if st.message_id.is_some() && st.last.elapsed() < Duration::from_secs(2) {
                return;
            }
            st.last = Instant::now();
            st.pending = true;
            (st.text.clone(), st.message_id, self.chat_id)
        };
        let tg = self.tg.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let result = match id {
                Some(id) => tg.edit_plain(chat_id, id, &snapshot).await.map(|_| id),
                None => tg.send_plain_id(chat_id, &snapshot).await,
            };
            let mut st = state.lock().unwrap();
            st.pending = false;
            if let Ok(id) = result {
                st.message_id = Some(id);
            }
        });
    }

    /// Deliver the final text. False means "nothing was shown, send it as a
    /// normal message"; true means the streamer owns delivery.
    pub async fn finish(&self, text: &str) -> Result<bool> {
        let id = {
            let st = self.state.lock().unwrap();
            st.message_id
        };
        let Some(id) = id else {
            return Ok(false);
        };
        let parts = chunks(text, TG_TEXT_LIMIT);
        let Some(first) = parts.first() else {
            return Ok(false);
        };
        let mut delivered = self.tg.edit_formatted(self.chat_id, id, first).await;
        if delivered.is_err() {
            delivered = self.tg.edit_plain(self.chat_id, id, first).await;
        }
        if let Err(e) = delivered {
            eprintln!("stream finish: {e:#}");
            return Ok(false);
        }
        for part in parts.iter().skip(1) {
            self.tg.send(self.chat_id, part).await?;
        }
        Ok(true)
    }
}

impl crate::llm::Sink for Streamer {
    fn delta(&self, text: &str) {
        Streamer::delta(self, text);
    }
}

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
    fn parses_approval_callbacks() {
        assert_eq!(parse_callback("approve:12"), Some((true, 12)));
        assert_eq!(parse_callback("deny:3"), Some((false, 3)));
        assert_eq!(parse_callback("approve:x"), None);
        assert_eq!(parse_callback("nope:1"), None);
        assert_eq!(parse_callback("approve"), None);
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
