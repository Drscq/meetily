-- Smart pipeline progress per meeting. status: processing | done | pending.
-- "pending" means a step failed and the meeting will be retried.
CREATE TABLE IF NOT EXISTS meeting_processing (
    meeting_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);
