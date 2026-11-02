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

-- Inbound calls never receive an outbound dial permit. One PIN attempt per SID.
CREATE TABLE IF NOT EXISTS phone_inbound_calls (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    pin_attempted INTEGER NOT NULL DEFAULT 0 CHECK(pin_attempted IN (0,1))
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

-- #1553 agent-proposed calls. A proposal is only a question posted to the
-- owner's private control chat; it never dials. `scheduled` waits for its
-- delivery time (quiet hours defer, never drop), `pending` awaits the owner's
-- yes/no until `expires_ms`. Every row counts toward the rate limits.
CREATE TABLE IF NOT EXISTS phone_call_proposals (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    conversation TEXT NOT NULL,
    origin_json TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(length(reason) BETWEEN 1 AND 300),
    agenda_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('scheduled','pending','accepted','declined','expired')),
    created_ms INTEGER NOT NULL,
    deliver_ms INTEGER NOT NULL,
    delivered_ms INTEGER,
    expires_ms INTEGER,
    decided_ms INTEGER,
    call_request TEXT
);
CREATE INDEX IF NOT EXISTS phone_call_proposals_owner ON phone_call_proposals(owner, deliver_ms);
CREATE INDEX IF NOT EXISTS phone_call_proposals_state ON phone_call_proposals(state, deliver_ms);
-- #1557 why a relayed call ended, from the relay-ended callback's handoffData,
-- or (#1556 'silence') from the relay itself. Written once; a replayed
-- callback cannot change it. `migrate` rebuilds a table created before
-- 'silence' was allowed.
CREATE TABLE IF NOT EXISTS phone_call_end_reasons (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    reason TEXT NOT NULL CHECK(reason IN ('owner_goodbye','task_complete','owner_request','relay_ended_unknown','silence')),
    recorded_ms INTEGER NOT NULL
);

-- #1556 the one chat notice for a call the relay ended itself. A lease
-- (attempts + next_ms) so a restart replays an unfinished delivery once,
-- under the same outbox idempotency key.
CREATE TABLE IF NOT EXISTS phone_call_end_notices (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','done')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 3),
    next_ms INTEGER NOT NULL DEFAULT 0,
    created_ms INTEGER NOT NULL,
    done_ms INTEGER
);

-- Durable voice delegation intent and delivery, independent of relay lifetime.
CREATE TABLE IF NOT EXISTS phone_dispatch_jobs (
    call_id TEXT NOT NULL REFERENCES phone_calls(id),
    task_id TEXT NOT NULL,
    description TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('queued','running','ready','done')),
    result TEXT,
    PRIMARY KEY(call_id, task_id)
);

-- One immutable provider session per durably claimed phone relay.
CREATE TABLE IF NOT EXISTS phone_live_sessions (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    session_id TEXT NOT NULL UNIQUE,
    active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1))
);
-- Separate audio receipt: chat delivery must not erase an unheard result.
CREATE TABLE IF NOT EXISTS phone_live_dispatches (
    call_id TEXT NOT NULL REFERENCES phone_live_sessions(call_id),
    delegation_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    result TEXT,
    delivered INTEGER NOT NULL DEFAULT 0 CHECK(delivered IN (0,1)),
    PRIMARY KEY(call_id,delegation_id),
    UNIQUE(call_id,task_id),
    FOREIGN KEY(call_id,task_id) REFERENCES phone_dispatch_jobs(call_id,task_id)
);

-- Backend choice is fixed before dialing; absent rows are legacy calls.
CREATE TABLE IF NOT EXISTS phone_voice_backends (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    backend TEXT NOT NULL CHECK(backend IN ('conversation_relay','openai_live'))
);

-- Private transcript outbox. Text is erased after durable chat acceptance;
-- fingerprints retain replay protection without retaining another transcript.
CREATE TABLE IF NOT EXISTS phone_live_history_batches (
    id TEXT PRIMARY KEY,
    attempt_order INTEGER NOT NULL DEFAULT 0,
    call_id TEXT NOT NULL REFERENCES phone_calls(id),
    text TEXT,
    delivered INTEGER NOT NULL DEFAULT 0 CHECK(delivered IN (0,1))
);
CREATE TABLE IF NOT EXISTS phone_live_transcripts (
    call_id TEXT NOT NULL REFERENCES phone_live_sessions(call_id),
    event_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    user INTEGER NOT NULL CHECK(user IN (0,1)),
    text TEXT,
    batch_id TEXT REFERENCES phone_live_history_batches(id),
    PRIMARY KEY(call_id,event_id)
);

-- One receipt per actual Live attempt; no final event means unknown usage.
CREATE TABLE IF NOT EXISTS phone_live_usage (
    call_id TEXT NOT NULL REFERENCES phone_calls(id),
    phase TEXT NOT NULL CHECK(phase IN ('preflight','conversation')),
    session_id TEXT UNIQUE,
    end_trigger TEXT,
    final_reason TEXT,
    seconds REAL CHECK(seconds IS NULL OR seconds>=0),
    error TEXT,
    finalization_error TEXT,
    PRIMARY KEY(call_id,phase)
);

-- Read-only spoken-summary retries never touch the native task state.
CREATE TABLE IF NOT EXISTS phone_live_summary_jobs (
    call_id TEXT NOT NULL,
    delegation_id TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    lease_token TEXT,
    next_ms INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(call_id,delegation_id),
    FOREIGN KEY(call_id,delegation_id) REFERENCES phone_live_dispatches(call_id,delegation_id)
);

-- Content-free playback checkpoints; collector closure is not proof of hearing.
CREATE TABLE IF NOT EXISTS phone_live_playback (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    evidence TEXT NOT NULL,
    collector_closed INTEGER NOT NULL CHECK(collector_closed IN (0,1))
);

-- A content-free warning survives capacity loss or private staging expiry.
CREATE TABLE IF NOT EXISTS phone_live_history_gaps (
    call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
    notice_delivered INTEGER NOT NULL DEFAULT 0 CHECK(notice_delivered IN (0,1)),
    attempt_order INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS phone_live_transcripts_pending
ON phone_live_transcripts(call_id) WHERE text IS NOT NULL;

-- Voice publication evidence is independent from native completion and chat.
-- Old delivered bits are deliberately not backfilled as provider acceptance.
CREATE TABLE IF NOT EXISTS phone_live_result_receipts (
    call_id TEXT NOT NULL,
    delegation_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK(disposition IN ('pending','sending','accepted','timed_out','discarded')),
    PRIMARY KEY(call_id,delegation_id),
    FOREIGN KEY(call_id,delegation_id) REFERENCES phone_live_dispatches(call_id,delegation_id)
);
CREATE TABLE IF NOT EXISTS phone_live_reply_intents (
    call_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    delegation_id TEXT NOT NULL,
    end_offset INTEGER NOT NULL CHECK(end_offset>0),
    complete INTEGER NOT NULL CHECK(complete IN (0,1)),
    disposition TEXT NOT NULL CHECK(disposition IN ('sending','accepted','rejected','timed_out','discarded')),
    PRIMARY KEY(call_id,event_id),
    FOREIGN KEY(call_id,delegation_id) REFERENCES phone_live_dispatches(call_id,delegation_id)
);

-- Completed fast read facts await the next already-requested native turn.
-- This is context, never a new reasoning/tool job and never a speech retry.
CREATE TABLE IF NOT EXISTS phone_attention_context (
    call_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    conversation_key TEXT NOT NULL,
    native_session_id TEXT,
    content TEXT,
    consumed_turn_id TEXT,
    PRIMARY KEY(call_id,task_id),
    FOREIGN KEY(call_id,task_id) REFERENCES phone_dispatch_jobs(call_id,task_id)
);
