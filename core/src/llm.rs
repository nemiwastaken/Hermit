use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// OpenAI-compatible chat completions. DeepSeek is the default target; any
/// endpoint with the same wire format works by changing LLM_BASE_URL.
pub struct Llm {
    http: reqwest::Client,
    url: String,
    key: String,
    model: String,
    max_tokens: Option<u64>,
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
        let max_tokens = std::env::var("LLM_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self {
            http,
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            key,
            model,
            max_tokens,
        })
    }

    pub async fn chat(&self, messages: &[Value], tools: Option<&Value>) -> Result<Reply> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });
        if let Some(t) = tools {
            body["tools"] = t.clone();
            body["tool_choice"] = json!("auto");
        }
        if let Some(mt) = self.max_tokens {
            body["max_tokens"] = json!(mt);
        }

        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&body)
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
