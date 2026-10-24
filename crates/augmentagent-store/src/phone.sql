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
CREATE TABLE IF NOT EXISTS phone_origins (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    origin_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS phone_origin_authority (
    call_id TEXT PRIMARY KEY REFERENCES phone_origins(call_id),
    revision INTEGER NOT NULL CHECK(revision > 0)
);

-- Separate additive table so existing call rows and native sessions survive upgrade.
CREATE TABLE IF NOT EXISTS phone_call_usage (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    duration_seconds INTEGER NOT NULL CHECK(duration_seconds >= 0),
    ended_ms INTEGER NOT NULL CHECK(ended_ms >= 0)
);

CREATE TABLE IF NOT EXISTS phone_usage_reconciliation (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 3),
    next_ms INTEGER NOT NULL DEFAULT 0
);

-- Conversational setup intent and replay receipts contain no phone numbers.
CREATE TABLE IF NOT EXISTS phone_setup_prompts (
    owner TEXT PRIMARY KEY,
    conversation TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS phone_number_receipts (
    owner TEXT NOT NULL,
    event_id TEXT NOT NULL,
    should_call INTEGER NOT NULL,
    PRIMARY KEY(owner, event_id)
);

-- #1551 per-turn latency record. Offsets are milliseconds relative to the final
-- prompt frame. No speech text, numbers or identifiers beyond call/turn IDs.
CREATE TABLE IF NOT EXISTS phone_turn_metrics (
    call_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    recorded_ms INTEGER NOT NULL CHECK(recorded_ms >= 0),
    provider TEXT NOT NULL CHECK(provider IN ('fast','native')),
    outcome TEXT NOT NULL CHECK(outcome IN ('answered','interrupted','failed','dispatched')),
    queue_wait_ms INTEGER CHECK(queue_wait_ms IS NULL OR queue_wait_ms >= 0),
    t_first_token_ms INTEGER CHECK(t_first_token_ms IS NULL OR t_first_token_ms >= 0),
    t_first_speech_ms INTEGER CHECK(t_first_speech_ms IS NULL OR t_first_speech_ms >= 0),
    t_last_speech_ms INTEGER CHECK(t_last_speech_ms IS NULL OR t_last_speech_ms >= 0),
    tool_ms INTEGER CHECK(tool_ms IS NULL OR tool_ms >= 0),
    t_first_played_ms INTEGER CHECK(t_first_played_ms IS NULL OR t_first_played_ms >= 0),
    t_last_played_ms INTEGER CHECK(t_last_played_ms IS NULL OR t_last_played_ms >= 0),
    audible_delta_ms INTEGER,
    commit_sha TEXT CHECK(commit_sha IS NULL OR length(commit_sha) BETWEEN 7 AND 40),
    PRIMARY KEY(call_id, turn_id)
);
CREATE INDEX IF NOT EXISTS phone_turn_metrics_recorded ON phone_turn_metrics(call_id, recorded_ms);

-- #1552 post-call memory. One durable summary job per relayed call; a claim is
-- a lease (attempts + next_ms) so a restart replays unfinished work once.
CREATE TABLE IF NOT EXISTS phone_call_summary_jobs (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','done')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 3),
    next_ms INTEGER NOT NULL DEFAULT 0,
    created_ms INTEGER NOT NULL,
    done_ms INTEGER,
    summary TEXT CHECK(summary IS NULL OR length(summary) <= 800),
    wiki_path TEXT
);
-- Owner-visible, owner-deletable facts learned from calls. No numbers or
-- credentials: values are short model-extracted statements.
CREATE TABLE IF NOT EXISTS owner_profile (
    owner TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    confidence REAL NOT NULL CHECK(confidence >= 0 AND confidence <= 1),
    source_call_id TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY(owner, key)
);
CREATE INDEX IF NOT EXISTS owner_profile_recent ON owner_profile(owner, updated_ms);

-- #1557 why a relayed call ended, from the relay-ended callback's handoffData.
-- Written once; a replayed callback cannot change it.
CREATE TABLE IF NOT EXISTS phone_call_end_reasons (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    reason TEXT NOT NULL CHECK(reason IN ('owner_goodbye','task_complete','owner_request','relay_ended_unknown')),
    recorded_ms INTEGER NOT NULL
);
