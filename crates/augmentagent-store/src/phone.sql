CREATE TABLE IF NOT EXISTS phone_destinations (
    owner TEXT PRIMARY KEY,
    number TEXT,
    updated_ms INTEGER NOT NULL,
    provenance TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS phone_calls (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    request_id TEXT NOT NULL,
    conversation TEXT NOT NULL,
    destination TEXT NOT NULL,
    verification INTEGER NOT NULL DEFAULT 0 CHECK(verification IN (0,1)),
    provider_sid TEXT UNIQUE,
    state TEXT NOT NULL CHECK(state IN ('requesting','uncertain','queued','ringing','awaiting_acceptance','connected','completed','busy','no_answer','canceled','failed')),
    created_ms INTEGER NOT NULL,
    answered_ms INTEGER,
    ended_ms INTEGER,
    max_duration_seconds INTEGER NOT NULL CHECK(max_duration_seconds BETWEEN 1 AND 900),
    stop_requested INTEGER NOT NULL DEFAULT 0 CHECK(stop_requested IN (0,1)),
    UNIQUE(owner,request_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS phone_one_active_owner ON phone_calls(owner) WHERE ended_ms IS NULL;
CREATE INDEX IF NOT EXISTS phone_owner_created ON phone_calls(owner,created_ms);

CREATE TABLE IF NOT EXISTS phone_verifications (
    owner TEXT PRIMARY KEY,
    call_id TEXT NOT NULL REFERENCES phone_calls(id),
    number TEXT NOT NULL,
    code_hash BLOB NOT NULL CHECK(length(code_hash)=32),
    expires_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS phone_relay_claims (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id)
);
CREATE TABLE IF NOT EXISTS phone_cleanup (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_ms INTEGER NOT NULL DEFAULT 0
);
