-- Numbers and facts the agent records over time. One table, optional number or
-- text, so weight, mood and "backup finished" all fit without migrations.
CREATE TABLE IF NOT EXISTS metrics (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id    INTEGER NOT NULL,
    metric_key TEXT NOT NULL,
    value_num  REAL,
    value_text TEXT,
    meta_json  TEXT NOT NULL DEFAULT '{}',
    ts         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS metrics_key ON metrics (chat_id, metric_key, ts);
