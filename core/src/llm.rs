use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

/// Receives text as it arrives, before the full answer exists.
pub trait Sink: Send + Sync {
    fn delta(&self, text: &str);
}

/// OpenAI-compatible chat completions. DeepSeek is the default target; any
/// endpoint with the same wire format works by changing LLM_BASE_URL.
pub struct Llm {
    http: reqwest::Client,
    url: String,
    key: String,
    model: String,
    vision: bool,
    max_tokens: Option<u64>,
    price_in: f64,
    price_out: f64,
}

pub struct Reply {
    /// The `choices[0].message` object, tool_calls included.
    pub message: Value,
    pub usage: Option<Value>,
}

impl Llm {
    pub fn from_env() -> Result<Self> {
        let base =
            std::env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com/v1".into());
        let key = std::env::var("LLM_API_KEY").context("LLM_API_KEY is required")?;
        if key.trim().is_empty() {
            bail!("LLM_API_KEY is empty");
        }
        let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
        let vision = std::env::var("LLM_VISION").as_deref() == Ok("1");
        let max_tokens = std::env::var("LLM_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok());
        // USD per 1M tokens; zero means "show tokens, do not guess a price".
        let price = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self {
            http,
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            key,
            model,
            vision,
            max_tokens,
            price_in: price("LLM_PRICE_IN_PER_M"),
            price_out: price("LLM_PRICE_OUT_PER_M"),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn vision(&self) -> bool {
        self.vision
    }

    /// Price of one call from its `usage` object. Unknown tokens cost zero.
    pub fn cost(&self, usage: &Value) -> f64 {
        let input = usage["prompt_tokens"].as_f64().unwrap_or(0.0);
        let output = usage["completion_tokens"].as_f64().unwrap_or(0.0);
        (input * self.price_in + output * self.price_out) / 1_000_000.0
    }

    fn body(&self, messages: &[Value], tools: Option<&Value>, stream: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": stream,
        });
        if stream {
            // Without this, OpenAI-compatible endpoints send no usage at all.
            body["stream_options"] = json!({"include_usage": true});
        }
        if let Some(t) = tools {
            body["tools"] = t.clone();
            body["tool_choice"] = json!("auto");
        }
        if let Some(mt) = self.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        body
    }

    pub async fn chat(&self, messages: &[Value], tools: Option<&Value>) -> Result<Reply> {
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&self.body(messages, tools, false))
            .send()
            .await
            .context("calling the LLM")?;

        let status = resp.status();
        let text = resp.text().await.context("reading the LLM response")?;
        if !status.is_success() {
            bail!(
                "LLM HTTP {status}: {}",
                text.chars().take(500).collect::<String>()
            );
        }
        let json: Value = serde_json::from_str(&text).context("LLM returned non-JSON")?;
        let message = json
            .pointer("/choices/0/message")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("LLM response has no choices[0].message"))?;
        Ok(Reply {
            message,
            usage: json.get("usage").cloned(),
        })
    }

    /// Same call with `stream: true`. Text is handed to `sink` as it arrives;
    /// tool-call arguments are assembled into one message. A provider that
    /// ignores `stream` (or refuses `stream_options` with 400) fails here, and
    /// the caller can retry with `chat` because nothing was shown yet.
    pub async fn chat_stream(
        &self,
        messages: &[Value],
        tools: Option<&Value>,
        sink: &dyn Sink,
    ) -> Result<Reply> {
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&self.body(messages, tools, true))
            .send()
            .await
            .context("calling the LLM")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "LLM HTTP {status}: {}",
                text.chars().take(500).collect::<String>()
            );
        }
        let event_stream = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("event-stream"));
        if !event_stream {
            let text = resp.text().await.context("reading the LLM response")?;
            let json: Value = serde_json::from_str(&text).context("LLM returned non-JSON")?;
            let message = json
                .pointer("/choices/0/message")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("LLM response has no choices[0].message"))?;
            if let Some(text) = message["content"].as_str() {
                sink.delta(text);
            }
            return Ok(Reply {
                message,
                usage: json.get("usage").cloned(),
            });
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut calls: Vec<Value> = Vec::new();
        let mut usage = None;
        let mut done = false;

        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.context("reading the LLM stream")?);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..pos])
                    .trim_end_matches('\r')
                    .to_string();
                buf.drain(..=pos);
                let Some(data) = line.trim_start().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(u) = v.get("usage")
                    && !u.is_null()
                {
                    usage = Some(u.clone());
                }
                let Some(delta) = v.pointer("/choices/0/delta") else {
                    continue;
                };
                if let Some(text) = delta["content"].as_str()
                    && !text.is_empty()
                {
                    content.push_str(text);
                    sink.delta(text);
                }
                if let Some(list) = delta["tool_calls"].as_array() {
                    for call in list {
                        merge_call(&mut calls, call);
                    }
                }
            }
            if done {
                break;
            }
        }

        let mut message = json!({"role": "assistant"});
        message["content"] = if content.is_empty() {
            Value::Null
        } else {
            json!(content)
        };
        if !calls.is_empty() {
            message["tool_calls"] = json!(calls);
        }
        Ok(Reply { message, usage })
    }
}

/// SSE sends tool calls in pieces: index identifies the call, the first piece
/// carries id and name, arguments arrive in fragments.
fn merge_call(calls: &mut Vec<Value>, piece: &Value) {
    let index = piece["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
    while calls.len() <= index {
        calls.push(json!({
            "id": "", "type": "function",
            "function": {"name": "", "arguments": ""}
        }));
    }
    let slot = &mut calls[index];
    if let Some(id) = piece["id"].as_str().filter(|s| !s.is_empty()) {
        slot["id"] = json!(id);
    }
    if let Some(name) = piece["function"]["name"].as_str().filter(|s| !s.is_empty()) {
        let current = slot["function"]["name"].as_str().unwrap_or("");
        if current.is_empty() {
            slot["function"]["name"] = json!(name);
        } else if current != name {
            slot["function"]["name"] = json!(format!("{current}{name}"));
        }
    }
    if let Some(args) = piece["function"]["arguments"].as_str() {
        let current = slot["function"]["arguments"].as_str().unwrap_or("");
        slot["function"]["arguments"] = json!(format!("{current}{args}"));
    }
}

/// Non-JSON text the model may wrap around a tool call, for the log.
pub fn brief(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() <= 300 {
        s
    } else {
        format!("{}…", s.chars().take(300).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_deltas_are_merged() {
        let mut calls = Vec::new();
        merge_call(
            &mut calls,
            &json!({"index": 0, "id": "call_1",
            "function": {"name": "read", "arguments": "{\"pa"}}),
        );
        merge_call(
            &mut calls,
            &json!({"index": 0,
            "function": {"arguments": "th\":\"a\"}"}}),
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "read");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a\"}");
    }

    #[test]
    fn cost_uses_per_million_prices() {
        let llm = Llm {
            http: reqwest::Client::new(),
            url: String::new(),
            key: String::new(),
            model: "m".into(),
            vision: false,
            max_tokens: None,
            price_in: 2.0,
            price_out: 8.0,
        };
        let usage = json!({"prompt_tokens": 1_000_000, "completion_tokens": 500_000});
        assert!((llm.cost(&usage) - 6.0).abs() < 1e-9);
    }
}
