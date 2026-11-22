use std::path::Path;
use rusqlite::Connection;

/// Seed the Node-tree-owned tables `Store::migrate` probes for. Mirrors
/// the helper in the lib's `tests` module — duplicated here because Rust
/// integration tests can't `use` private items from the library crate.
pub fn seed_node_owned_tables(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS actions (
            id TEXT PRIMARY KEY,
            messageId TEXT NOT NULL,
            threadId TEXT,
            fromEmail TEXT NOT NULL,
            subject TEXT NOT NULL,
            originalBody TEXT,
            draftBody TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            errorMessage TEXT,
            createdAt INTEGER NOT NULL,
            updatedAt INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS emails (
            messageId TEXT PRIMARY KEY,
            threadId TEXT,
            fromEmail TEXT NOT NULL,
            subject TEXT NOT NULL,
            body TEXT,
            receivedAt TEXT,
            accountEntityId TEXT,
            firstSeenAt INTEGER NOT NULL,
            triageResult TEXT,
            agentProcessedAt INTEGER,
            platform TEXT NOT NULL DEFAULT 'gmail',
            kind TEXT NOT NULL DEFAULT 'dm'
        );
        CREATE TABLE IF NOT EXISTS gmail_accounts (
            id TEXT PRIMARY KEY,
            connectionId TEXT NOT NULL,
            email TEXT,
            label TEXT,
            entityId TEXT NOT NULL,
            active INTEGER DEFAULT 1,
            createdAt INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS channel_subscriptions (
            id TEXT PRIMARY KEY,
            platform TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            display_name TEXT NOT NULL,
            mode TEXT NOT NULL,
            active INTEGER NOT NULL DEFAULT 1,
            last_seen_message_id TEXT,
            last_digest_at_ms INTEGER,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS slack_workspaces (
            id TEXT PRIMARY KEY,
            team_id TEXT NOT NULL UNIQUE,
            team_name TEXT NOT NULL,
            entity_id TEXT NOT NULL,
            connection_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            active INTEGER NOT NULL DEFAULT 1,
            created_at_ms INTEGER NOT NULL
        );
        "#,
    )
    .unwrap();
}
