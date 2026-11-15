PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'new', -- new | running | done | dead
    attempts   INTEGER NOT NULL DEFAULT 0,
    dead_at    TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_state ON events (state, id);

-- Conversation history per chat, as a JSON array of OpenAI-style messages.
CREATE TABLE IF NOT EXISTS sessions (
    chat_id    INTEGER PRIMARY KEY,
    history    TEXT NOT NULL DEFAULT '[]',
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
