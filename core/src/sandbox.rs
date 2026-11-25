use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The only way core touches the machine. Requests and response headers use JSON lines.
pub struct Sandbox {
    socket: String,
    timeout: Duration,
}

impl Sandbox {
    pub fn new(socket: impl Into<String>) -> Self {
        // The container applies its own timeout_ms; this is just the outer guard.
        Self::with_timeout(socket, 660)
    }

    /// The net container answers in seconds, so a hung one must not hold the
    /// worker for eleven minutes.
    pub fn with_timeout(socket: impl Into<String>, secs: u64) -> Self {
        Self {
            socket: socket.into(),
            timeout: Duration::from_secs(secs),
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

    /// One attempt. Use it for tools that must not run twice when a reply is
    /// lost: file writes, process start, MCP calls and execute_code.
    pub async fn call_once(&self, id: &str, tool: &str, args: &Value) -> Result<Value> {
        let mut reader = self.request(id, tool, args).await?;
        let mut buf = String::new();
        let n = tokio::time::timeout(self.timeout, reader.read_line(&mut buf))
            .await
            .context("sandbox did not answer in time")??;
        if n == 0 {
            bail!("sandbox closed the connection");
        }
        serde_json::from_str(&buf).context("sandbox returned invalid JSON")
    }

    pub async fn export_file(&self, id: &str, path: &str) -> Result<(String, Vec<u8>)> {
        let mut reader = self
            .request(id, "export_file", &json!({"path": path}))
            .await?;
        tokio::time::timeout(Duration::from_secs(60), async {
            let mut line = String::new();
            (&mut reader).take(4096).read_line(&mut line).await?;
            if !line.ends_with('\n') {
                bail!("sandbox returned an incomplete file header");
            }
            let header: Value = serde_json::from_str(&line)?;
            if header["id"] != id {
                bail!("sandbox returned an unexpected file response");
            }
            if header["ok"] != true {
                bail!(
                    "{}",
                    header["error"]
                        .as_str()
                        .unwrap_or("sandbox could not open the file")
                );
            }
            let filename = header["filename"]
                .as_str()
                .filter(|name| {
                    !name.is_empty() && !name.contains('/') && !name.chars().any(char::is_control)
                })
                .context("invalid filename in sandbox file header")?;
            let size = header["size"]
                .as_u64()
                .context("missing sandbox file size")?;
            if size > crate::telegram::MAX_DOCUMENT_BYTES as u64 {
                bail!("file exceeds the 50 MB Telegram upload limit");
            }
            let mut bytes = vec![0; size as usize];
            reader
                .read_exact(&mut bytes)
                .await
                .context("sandbox file transfer was incomplete")?;
            Ok((filename.to_owned(), bytes))
        })
        .await
        .context("sandbox file transfer timed out")?
    }

    async fn request(&self, id: &str, tool: &str, args: &Value) -> Result<BufReader<UnixStream>> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to sandbox at {}", self.socket))?;
        let mut line = serde_json::to_string(&json!({"id": id, "tool": tool, "args": args}))?;
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;
        Ok(BufReader::new(stream))
    }
}
