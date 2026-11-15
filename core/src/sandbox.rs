use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The only way core touches the machine. One request, one response, JSON line.
pub struct Sandbox {
    socket: String,
    timeout: Duration,
}

impl Sandbox {
    pub fn new(socket: impl Into<String>) -> Self {
        // The container applies its own timeout_ms; this is just the outer guard.
        Self {
            socket: socket.into(),
            timeout: Duration::from_secs(660),
        }
    }

    /// Retries only the connection: a tool that already ran is never re-run.
    pub async fn call(&self, id: &str, tool: &str, args: &Value) -> Result<Value> {
        let mut last = None;
        for attempt in 0..3u64 {
            match self.call_once(id, tool, args).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = format!("{e:#}");
                    last = Some(anyhow::anyhow!(msg));
                    tokio::time::sleep(Duration::from_millis(300 * (attempt + 1))).await;
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("sandbox unavailable")))
    }

    async fn call_once(&self, id: &str, tool: &str, args: &Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to sandbox at {}", self.socket))?;
        let (rd, mut wr) = stream.into_split();

        let mut line = serde_json::to_string(&json!({"id": id, "tool": tool, "args": args}))?;
        line.push('\n');
        wr.write_all(line.as_bytes()).await?;
        wr.flush().await?;

        let mut reader = BufReader::new(rd);
        let mut buf = String::new();
        let n = tokio::time::timeout(self.timeout, reader.read_line(&mut buf))
            .await
            .context("sandbox did not answer in time")??;
        if n == 0 {
            bail!("sandbox closed the connection");
        }
        serde_json::from_str(&buf).context("sandbox returned invalid JSON")
    }
}
