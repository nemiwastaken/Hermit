-- Closed sessions. `history` is what was in the active context at closing
-- time; `task` and `final_answer` survive the trimming of that history.
CREATE TABLE IF NOT EXISTS archive (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id      INTEGER NOT NULL,
    title        TEXT NOT NULL,
    task         TEXT NOT NULL,
    final_answer TEXT NOT NULL,
    history      TEXT NOT NULL,
    created_at   TEXT NOT NULL, -- when the session was closed
    updated_at   TEXT NOT NULL  -- last message in that session
);
CREATE INDEX IF NOT EXISTS archive_chat ON archive (chat_id, updated_at DESC);
