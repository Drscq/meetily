-- User glossary used to bias post-meeting transcript correction.
-- term_key is the case-folded term; deleted_at keeps a tombstone so a term
-- the user removed is never re-learned automatically.
CREATE TABLE IF NOT EXISTS glossary_terms (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    term TEXT NOT NULL,
    term_key TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    deleted_at TEXT
);

-- Terms proposed by the correction model but not yet promoted.
CREATE TABLE IF NOT EXISTS glossary_candidates (
    term_key TEXT NOT NULL,
    term TEXT NOT NULL,
    meeting_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (term_key, meeting_id)
);

INSERT OR IGNORE INTO glossary_terms (term, term_key, source) VALUES
    ('Thang Hoang', 'thang hoang', 'default'),
    ('MATTER protocol', 'matter protocol', 'default');
