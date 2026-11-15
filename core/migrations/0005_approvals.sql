-- A paused agent turn waiting for the user to approve a risky bash call.
-- `calls` and `history` are JSON: the model must see all tool results before
-- it is called again, so the whole batch is stored, not just the risky call.
CREATE TABLE IF NOT EXISTS approvals (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id    INTEGER NOT NULL,
    calls      TEXT NOT NULL,
    label      TEXT NOT NULL,
    step       INTEGER NOT NULL,
    history    TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS approvals_chat ON approvals (chat_id);
