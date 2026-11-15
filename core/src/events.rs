use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

// shortcut: no automatic retry yet. Re-running a run that already executed
// tools can duplicate side effects, so a failed event parks in the DLQ instead.
// Raise this once tool calls are recorded and can be replayed safely.
pub const MAX_ATTEMPTS: i64 = 1;

#[derive(Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub kind: String,
    pub payload: String,
    pub attempts: i64,
}

/// One SQLite connection behind a mutex. The worker is serial, so a pool is
/// pointless here; WAL still lets the telegram poller write while it runs.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> Result<Arc<Self>> {
        if let Some(dir) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {path}"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // journal_mode returns a row, so it cannot go through pragma_update.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(include_str!("../migrations/0001_init.sql"))?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push_event(&self, kind: &str, payload: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO events (kind, payload, created_at) VALUES (?1, ?2, ?3)",
            params![kind, payload, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Atomically take the oldest pending event. Returns None when idle.
    pub fn claim_event(&self) -> Result<Option<Event>> {
        let conn = self.lock();
        let ev = conn
            .query_row(
                "UPDATE events SET state = 'running', attempts = attempts + 1
                 WHERE id = (SELECT id FROM events WHERE state = 'new' ORDER BY id LIMIT 1)
                 RETURNING id, kind, payload, attempts",
                [],
                |r| {
                    Ok(Event {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        payload: r.get(2)?,
                        attempts: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(ev)
    }

    pub fn finish(&self, id: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE events SET state = 'done' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Retry until MAX_ATTEMPTS, then park the event in the DLQ (state = dead).
    pub fn fail(&self, id: i64, err: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE events SET
                 state = CASE WHEN attempts >= ?2 THEN 'dead' ELSE 'new' END,
                 dead_at = CASE WHEN attempts >= ?2 THEN ?3 ELSE NULL END
             WHERE id = ?1",
            params![id, MAX_ATTEMPTS, now()],
        )?;
        eprintln!("event {id} failed: {err}");
        Ok(())
    }

    pub fn history(&self, chat_id: i64) -> Result<Vec<serde_json::Value>> {
        let raw: Option<String> = self
            .lock()
            .query_row(
                "SELECT history FROM sessions WHERE chat_id = ?1",
                params![chat_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default())
    }

    pub fn save_history(&self, chat_id: i64, history: &[serde_json::Value]) -> Result<()> {
        self.lock().execute(
            "INSERT INTO sessions (chat_id, history, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id) DO UPDATE SET history = ?2, updated_at = ?3",
            params![chat_id, serde_json::to_string(history)?, now()],
        )?;
        Ok(())
    }

    pub fn clear_history(&self, chat_id: i64) -> Result<()> {
        self.lock()
            .execute("DELETE FROM sessions WHERE chat_id = ?1", params![chat_id])?;
        Ok(())
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// An event left in 'running' by a crash would otherwise never be retried.
    pub fn recover_running(&self) -> Result<usize> {
        Ok(self.lock().execute(
            "UPDATE events SET state = 'new' WHERE state = 'running'",
            [],
        )?)
    }

    pub fn queue_len(&self) -> Result<i64> {
        Ok(self.lock().query_row(
            "SELECT count(*) FROM events WHERE state IN ('new','running')",
            [],
            |r| r.get(0),
        )?)
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
