-- Reminders. The agent turns "напоминай в 10:00" into a row; the worker turns
-- due rows into normal queue events, so they go through the usual agent loop.
CREATE TABLE IF NOT EXISTS schedules (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id    INTEGER NOT NULL,
    text       TEXT NOT NULL,                 -- what the agent receives when due
    next_at    TEXT NOT NULL,                 -- RFC3339, UTC
    repeat     TEXT NOT NULL DEFAULT 'none',  -- none | daily | weekly
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS schedules_due ON schedules (next_at);
