use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::cmd::Flags;
use crate::events::Db;
use crate::llm::Llm;
use crate::policy;
use crate::prompt;
use crate::sandbox::Sandbox;

/// Hard stop for a runaway tool loop. The model gets one warning per run,
/// the queue gets its answer, and the chat stays responsive.
const MAX_STEPS: usize = 12;
/// Messages kept per chat, newest last. Older ones are dropped before the call.
const KEEP_MESSAGES: usize = 30;
/// One tool result must not fill the whole context window.
const MAX_TOOL_CHARS: usize = 8_000;

pub struct Agent {
    pub llm: Llm,
    pub tools: Sandbox,
    pub db: Arc<Db>,
    pub flags: Arc<Flags>,
    pub behavior_dir: String,
}

impl Agent {
    /// The whole agent loop: assemble → LLM → policy → tools → repeat.
    pub async fn reply(&self, event_id: i64, chat_id: i64, text: &str) -> Result<String> {
        let mut history = self.db.history(chat_id)?;
        history.retain(|m| m["role"] != "system");
        history.insert(
            0,
            json!({"role": "system", "content": prompt::system(&self.behavior_dir)}),
        );
        history.push(json!({"role": "user", "content": text}));

        for step in 0..MAX_STEPS {
            let tools = if self.flags.safe() {
                None
            } else {
                Some(policy::schema())
            };
            let reply = self.llm.chat(&history, tools.as_ref()).await?;
            if let Some(u) = &reply.usage {
                eprintln!("usage: {u}");
            }
            let msg = reply.message;
            let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
            history.push(msg.clone());

            if calls.is_empty() {
                let content = msg["content"].as_str().unwrap_or("").trim().to_string();
                trim(&mut history);
                self.db.save_history(chat_id, &history)?;
                return Ok(if content.is_empty() {
                    "(модель вернула пустой ответ)".into()
                } else {
                    content
                });
            }

            for call in calls {
                let call_id = call["id"].as_str().unwrap_or("call").to_string();
                let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
                let args = match serde_json::from_str::<Value>(raw) {
                    Ok(v) if v.is_object() => v,
                    _ => json!({"__unparsable_arguments": raw}),
                };

                let result = self
                    .run_tool(&format!("e{event_id}/{step}"), &name, &args)
                    .await;
                eprintln!("tool {name} -> {}", crate::llm::brief(&result));
                history.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": clamp(&result.to_string(), MAX_TOOL_CHARS),
                }));
            }
            trim(&mut history);
        }

        self.db.save_history(chat_id, &history)?;
        Ok(format!(
            "Не уложился в {MAX_STEPS} шагов, похоже на цикл. Уточни задачу или /clear."
        ))
    }

    async fn run_tool(&self, id: &str, name: &str, args: &Value) -> Value {
        if let Err(e) = policy::check(name, self.flags.safe()) {
            return json!({"ok": false, "error": format!("{e:#}")});
        }
        match self.tools.call(id, name, args).await {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
        }
    }
}

/// Keep the system prompt and the newest messages. A leading orphan `tool`
/// message would be rejected by the API, so drop those too.
fn trim(history: &mut Vec<Value>) {
    let keep_from = 1 + history[1..].len().saturating_sub(KEEP_MESSAGES);
    history.drain(1..keep_from);
    while history.len() > 1 && history[1]["role"] == "tool" {
        history.remove(1);
    }
}

fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("… (обрезано)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_system_and_drops_orphan_tool() {
        let mut h = vec![json!({"role":"system","content":"s"})];
        for i in 0..40 {
            h.push(json!({"role":"user","content":format!("u{i}")}));
        }
        h.push(json!({"role":"tool","content":"orphan"}));
        h.push(json!({"role":"user","content":"last"}));
        trim(&mut h);
        assert_eq!(h[0]["role"], "system");
        assert_ne!(h[1]["role"], "tool");
        assert_eq!(h.last().unwrap()["content"], "last");
        assert!(h.len() <= 31, "{}", h.len());
    }
}
