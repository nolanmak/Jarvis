-- Read coverage and automation eligibility have deliberately separate state.
CREATE TABLE IF NOT EXISTS linkedin_archive_mailboxes (
 account TEXT NOT NULL, category TEXT NOT NULL, cursor TEXT,
 complete INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 0,
 last_success_ms INTEGER, last_error TEXT, retry_at_ms INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(account,category)
);
CREATE TABLE IF NOT EXISTS linkedin_archive_conversations (
 account TEXT NOT NULL, urn TEXT NOT NULL, title TEXT NOT NULL,
 group_chat INTEGER NOT NULL, latest_ms INTEGER NOT NULL,
 cursor TEXT, complete INTEGER NOT NULL DEFAULT 0,
 revision INTEGER NOT NULL DEFAULT 0, last_success_ms INTEGER,
 last_error TEXT, retry_at_ms INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(account,urn)
);
CREATE TABLE IF NOT EXISTS linkedin_archive_categories (
 account TEXT NOT NULL, category TEXT NOT NULL, conversation TEXT NOT NULL,
 PRIMARY KEY(account,category,conversation)
);
CREATE TABLE IF NOT EXISTS linkedin_archive_participants (
 account TEXT NOT NULL, conversation TEXT NOT NULL, urn TEXT NOT NULL,
 name TEXT NOT NULL, observed_at_ms INTEGER NOT NULL,
 PRIMARY KEY(account,conversation,urn)
);
CREATE INDEX IF NOT EXISTS linkedin_archive_participant_lookup
 ON linkedin_archive_participants(urn,account,conversation);
CREATE INDEX IF NOT EXISTS linkedin_archive_participant_name
 ON linkedin_archive_participants(name COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS linkedin_archive_messages (
 account TEXT NOT NULL, urn TEXT NOT NULL, conversation TEXT NOT NULL,
 stored_id TEXT NOT NULL UNIQUE, sender TEXT NOT NULL, from_me INTEGER NOT NULL,
 source_ms INTEGER NOT NULL, kind TEXT NOT NULL, content_json TEXT NOT NULL,
 PRIMARY KEY(account,urn)
);
CREATE INDEX IF NOT EXISTS linkedin_archive_thread
 ON linkedin_archive_messages(account,conversation,source_ms,urn);
CREATE TABLE IF NOT EXISTS linkedin_archive_cursors (
 account TEXT NOT NULL, scope TEXT NOT NULL, cursor TEXT NOT NULL,
 PRIMARY KEY(account,scope,cursor)
);
-- Live triage starts at an explicit persisted boundary, independent of history.
CREATE TABLE IF NOT EXISTS linkedin_archive_live_state (
 account TEXT PRIMARY KEY, started_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS linkedin_archive_live_heads (
 account TEXT NOT NULL, conversation TEXT NOT NULL,
 source_ms INTEGER NOT NULL, urn TEXT NOT NULL,
 PRIMARY KEY(account,conversation)
);
CREATE TABLE IF NOT EXISTS linkedin_archive_live_queue (
 account TEXT NOT NULL, conversation TEXT NOT NULL, stored_id TEXT NOT NULL UNIQUE,
 claim_token TEXT, claimed_at_ms INTEGER,
 PRIMARY KEY(account,conversation)
);
CREATE TABLE IF NOT EXISTS linkedin_archive_live_seen (
 account TEXT NOT NULL, urn TEXT NOT NULL,
 PRIMARY KEY(account,urn)
);
-- One automatic cursor recovery per reconciliation, including across restarts.
CREATE TABLE IF NOT EXISTS linkedin_archive_restarts (
 account TEXT NOT NULL, scope TEXT NOT NULL, restarted_at_ms INTEGER NOT NULL,
 PRIMARY KEY(account,scope)
);
