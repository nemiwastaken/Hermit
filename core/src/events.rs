use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

/// Sessions are forgotten this many days after their last message.
pub const ARCHIVE_DAYS: i64 = 30;

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

/// A closed session without its transcript: enough for listing and search.
#[derive(Debug, Clone)]
pub struct Archived {
    pub id: i64,
    pub title: String,
    pub task: String,
    pub final_answer: String,
    pub updated_at: String,
}

pub struct ArchivedFull {
    pub info: Archived,
    pub history: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct Schedule {
    pub id: i64,
    pub chat_id: i64,
    pub text: String,
    pub next_at: String,
    pub repeat: String,
}

#[derive(Debug, Clone)]
pub struct Metric {
    pub value_num: Option<f64>,
    pub value_text: Option<String>,
    pub meta: String,
    pub ts: String,
}

#[derive(Debug, Clone)]
pub struct MetricStats {
    pub count: i64,
    pub sum: f64,
    pub avg: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub last_num: Option<f64>,
    pub last_text: Option<String>,
    pub last_ts: Option<String>,
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
        conn.execute_batch(include_str!("../migrations/0002_archive.sql"))?;
        conn.execute_batch(include_str!("../migrations/0003_schedules.sql"))?;
        conn.execute_batch(include_str!("../migrations/0004_metrics.sql"))?;
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

    /// Messages and the time of the last one (RFC3339, UTC).
    pub fn history(&self, chat_id: i64) -> Result<(Vec<Value>, Option<String>)> {
        let row: Option<(String, String)> = self
            .lock()
            .query_row(
                "SELECT history, updated_at FROM sessions WHERE chat_id = ?1",
                params![chat_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((h, at)) => (serde_json::from_str(&h).unwrap_or_default(), Some(at)),
            None => (Vec::new(), None),
        })
    }

    pub fn save_history(&self, chat_id: i64, history: &[Value]) -> Result<()> {
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

    /// `last_activity` is the last message of the session being closed, not the
    /// moment it is closed: retention counts from the message.
    pub fn archive(
        &self,
        chat_id: i64,
        title: &str,
        task: &str,
        final_answer: &str,
        history: &[Value],
        last_activity: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO archive
                 (chat_id, title, task, final_answer, history, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                chat_id,
                title,
                task,
                final_answer,
                serde_json::to_string(history)?,
                now(),
                last_activity
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn archived(&self, chat_id: i64, id: i64) -> Result<Option<ArchivedFull>> {
        let row = self
            .lock()
            .query_row(
                "SELECT id, title, task, final_answer, updated_at, history
                 FROM archive WHERE chat_id = ?1 AND id = ?2",
                params![chat_id, id],
                |r| {
                    Ok((
                        Archived {
                            id: r.get(0)?,
                            title: r.get(1)?,
                            task: r.get(2)?,
                            final_answer: r.get(3)?,
                            updated_at: r.get(4)?,
                        },
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(info, history)| ArchivedFull {
            info,
            history: serde_json::from_str(&history).unwrap_or_default(),
        }))
    }

    /// Everything without transcripts, newest first. LIMIT -1 means all rows.
    pub fn catalog(&self, chat_id: i64) -> Result<Vec<Archived>> {
        self.list_archived(chat_id, -1)
    }

    pub fn recent_archived(&self, chat_id: i64, limit: usize) -> Result<Vec<Archived>> {
        self.list_archived(chat_id, limit as i64)
    }

    fn list_archived(&self, chat_id: i64, limit: i64) -> Result<Vec<Archived>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, title, task, final_answer, updated_at FROM archive
             WHERE chat_id = ?1 ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![chat_id, limit], |r| {
            Ok(Archived {
                id: r.get(0)?,
                title: r.get(1)?,
                task: r.get(2)?,
                final_answer: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn rename_archived(&self, chat_id: i64, id: i64, title: &str) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE archive SET title = ?3 WHERE chat_id = ?1 AND id = ?2",
            params![chat_id, id, title],
        )?;
        Ok(n > 0)
    }

    pub fn delete_archived(&self, chat_id: i64, id: i64) -> Result<bool> {
        let n = self.lock().execute(
            "DELETE FROM archive WHERE chat_id = ?1 AND id = ?2",
            params![chat_id, id],
        )?;
        Ok(n > 0)
    }

    /// Move an archived session back into the active slot. One transaction, so
    /// a crash cannot leave both or neither.
    pub fn resume_archived(&self, chat_id: i64, id: i64) -> Result<bool> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let history: Option<String> = tx
            .query_row(
                "SELECT history FROM archive WHERE chat_id = ?1 AND id = ?2",
                params![chat_id, id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(history) = history else {
            return Ok(false);
        };
        tx.execute(
            "INSERT INTO sessions (chat_id, history, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id) DO UPDATE SET history = ?2, updated_at = ?3",
            params![chat_id, history, now()],
        )?;
        tx.execute(
            "DELETE FROM archive WHERE chat_id = ?1 AND id = ?2",
            params![chat_id, id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn archive_count(&self, chat_id: i64) -> Result<i64> {
        Ok(self.lock().query_row(
            "SELECT count(*) FROM archive WHERE chat_id = ?1",
            params![chat_id],
            |r| r.get(0),
        )?)
    }

    /// Retention. Rows store our own UTC `+00:00` timestamps, so a string
    /// comparison against an RFC3339 cutoff is exact.
    pub fn purge_archive(&self, days: i64) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        Ok(self
            .lock()
            .execute("DELETE FROM archive WHERE updated_at < ?1", params![cutoff])?)
    }

    pub fn schedule_add(
        &self,
        chat_id: i64,
        text: &str,
        next_at: &str,
        repeat: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO schedules (chat_id, text, next_at, repeat, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![chat_id, text, next_at, repeat, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn schedules(&self, chat_id: i64) -> Result<Vec<Schedule>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, text, next_at, repeat FROM schedules
             WHERE chat_id = ?1 ORDER BY next_at",
        )?;
        let rows = stmt.query_map(params![chat_id], schedule_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Across all chats: the worker has no single chat. Oldest deadline first.
    pub fn due_schedules(&self, now: &str) -> Result<Vec<Schedule>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, text, next_at, repeat FROM schedules
             WHERE next_at <= ?1 ORDER BY next_at",
        )?;
        let rows = stmt.query_map(params![now], schedule_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn schedule_advance(&self, id: i64, next_at: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE schedules SET next_at = ?2 WHERE id = ?1",
            params![id, next_at],
        )?;
        Ok(())
    }

    pub fn schedule_delete(&self, chat_id: i64, id: i64) -> Result<bool> {
        let n = self.lock().execute(
            "DELETE FROM schedules WHERE chat_id = ?1 AND id = ?2",
            params![chat_id, id],
        )?;
        Ok(n > 0)
    }

    pub fn metric_add(
        &self,
        chat_id: i64,
        key: &str,
        value_num: Option<f64>,
        value_text: Option<&str>,
        meta: &str,
        ts: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO metrics (chat_id, metric_key, value_num, value_text, meta_json, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![chat_id, key, value_num, value_text, meta, ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn metric_rows(
        &self,
        chat_id: i64,
        key: &str,
        since: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Metric>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT value_num, value_text, meta_json, ts FROM metrics
             WHERE chat_id = ?1 AND metric_key = ?2 AND (?3 IS NULL OR ts >= ?3)
             ORDER BY ts DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![chat_id, key, since, limit as i64], |r| {
            Ok(Metric {
                value_num: r.get(0)?,
                value_text: r.get(1)?,
                meta: r.get(2)?,
                ts: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn metric_stats(
        &self,
        chat_id: i64,
        key: &str,
        since: Option<&str>,
    ) -> Result<MetricStats> {
        let conn = self.lock();
        let (count, sum, avg, min, max, last_ts) = conn.query_row(
            "SELECT count(*), coalesce(sum(value_num), 0), coalesce(avg(value_num), 0),
                    min(value_num), max(value_num), max(ts)
             FROM metrics
             WHERE chat_id = ?1 AND metric_key = ?2 AND (?3 IS NULL OR ts >= ?3)",
            params![chat_id, key, since],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
        let last: Option<(Option<f64>, Option<String>)> = conn
            .query_row(
                "SELECT value_num, value_text FROM metrics
                 WHERE chat_id = ?1 AND metric_key = ?2 AND (?3 IS NULL OR ts >= ?3)
                 ORDER BY ts DESC, id DESC LIMIT 1",
                params![chat_id, key, since],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (last_num, last_text) = last.unwrap_or((None, None));
        Ok(MetricStats {
            count,
            sum,
            avg,
            min,
            max,
            last_num,
            last_text,
            last_ts,
        })
    }

    /// Keys seen in the window, most recently used first.
    pub fn metric_keys(
        &self,
        chat_id: i64,
        since: Option<&str>,
    ) -> Result<Vec<(String, i64, Option<String>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT metric_key, count(*), max(ts) FROM metrics
             WHERE chat_id = ?1 AND (?2 IS NULL OR ts >= ?2)
             GROUP BY metric_key ORDER BY max(ts) DESC",
        )?;
        let rows = stmt.query_map(params![chat_id, since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

    pub fn kv_del(&self, key: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM kv WHERE key = ?1", params![key])?;
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

fn schedule_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get(0)?,
        chat_id: r.get(1)?,
        text: r.get(2)?,
        next_at: r.get(3)?,
        repeat: r.get(4)?,
    })
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// `kv` keys for the active session.
pub fn task_key(chat_id: i64) -> String {
    format!("task:{chat_id}")
}

pub fn recap_key(chat_id: i64) -> String {
    format!("recap:{chat_id}")
}

pub fn recap_id_key(chat_id: i64) -> String {
    format!("recap_id:{chat_id}")
}

/// True only when the timestamp parses and is older than `days`. An
/// unreadable timestamp is treated as "unknown", never as expired.
pub fn expired(updated_at: &str, now: chrono::DateTime<chrono::Utc>, days: i64) -> bool {
    match chrono::DateTime::parse_from_rfc3339(updated_at) {
        Ok(t) => t.with_timezone(&chrono::Utc) < now - chrono::Duration::days(days),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OLD: &str = "2020-01-01T00:00:00+00:00";

    fn open(name: &str) -> (Arc<Db>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("hermit-db-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();
        (db, dir)
    }

    #[test]
    fn archive_roundtrip_resume_and_isolation() {
        let (db, dir) = open("archive");
        let h = vec![json!({"role":"user","content":"fix docker"})];
        let id = db
            .archive(1, "Fixed docker", "fix docker", "done", &h, OLD)
            .unwrap();
        assert_eq!(db.archive_count(1).unwrap(), 1);
        assert!(db.catalog(2).unwrap().is_empty(), "chats are isolated");

        let full = db.archived(1, id).unwrap().unwrap();
        assert_eq!(full.info.title, "Fixed docker");
        assert_eq!(full.history, h);

        assert!(db.rename_archived(1, id, "Docker fixed").unwrap());
        assert_eq!(db.catalog(1).unwrap()[0].title, "Docker fixed");
        assert!(!db.rename_archived(2, id, "nope").unwrap());

        db.save_history(1, &h).unwrap();
        assert!(db.resume_archived(1, id).unwrap());
        assert_eq!(db.history(1).unwrap().0, h);
        assert_eq!(db.archive_count(1).unwrap(), 0);
        assert!(!db.resume_archived(1, id).unwrap());

        let id = db.archive(1, "two", "t", "f", &h, OLD).unwrap();
        assert!(db.delete_archived(1, id).unwrap());
        assert!(!db.delete_archived(1, id).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn purge_removes_only_old_and_history_reports_time() {
        let (db, dir) = open("purge");
        let h = vec![json!({"role":"user","content":"x"})];
        db.archive(1, "old", "t", "f", &h, OLD).unwrap();
        db.archive(1, "new", "t", "f", &h, &now()).unwrap();
        assert_eq!(db.purge_archive(ARCHIVE_DAYS).unwrap(), 1);
        let left = db.catalog(1).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].title, "new");

        assert_eq!(db.history(1).unwrap().1, None);
        db.save_history(1, &h).unwrap();
        assert!(db.history(1).unwrap().1.is_some());
        assert_eq!(db.history(1).unwrap().0, h);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schedules_roundtrip_due_and_isolation() {
        let (db, dir) = open("sched");
        let id = db
            .schedule_add(1, "backup", "2026-01-01T10:00:00+00:00", "daily")
            .unwrap();
        assert_eq!(db.schedules(1).unwrap().len(), 1);
        assert!(db.schedules(2).unwrap().is_empty(), "chats are isolated");
        assert!(
            db.due_schedules("2026-01-01T09:59:00+00:00")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.due_schedules("2026-01-02T00:00:00+00:00").unwrap()[0].id,
            id
        );
        db.schedule_advance(id, "2026-01-03T10:00:00+00:00")
            .unwrap();
        assert!(
            db.due_schedules("2026-01-02T12:00:00+00:00")
                .unwrap()
                .is_empty(),
            "advance moves the deadline"
        );
        assert!(!db.schedule_delete(2, id).unwrap());
        assert!(db.schedule_delete(1, id).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kv_delete_and_expired() {
        let (db, dir) = open("kv");
        db.kv_set("task:1", "fix").unwrap();
        assert_eq!(db.kv_get("task:1").unwrap().as_deref(), Some("fix"));
        db.kv_del("task:1").unwrap();
        assert_eq!(db.kv_get("task:1").unwrap(), None);

        let now = chrono::Utc::now();
        assert!(expired(OLD, now, ARCHIVE_DAYS));
        assert!(!expired(&now.to_rfc3339(), now, ARCHIVE_DAYS));
        assert!(!expired("не дата", now, ARCHIVE_DAYS));
        std::fs::remove_dir_all(&dir).ok();
    }
}
