-- Pre-#1299 Slack state, produced by augmentagent-store at 6ee3485 (origin/main
-- before #1299): Store::open on an empty file, then the Slack install/owner/
-- delivery/health APIs with synthetic identifiers. Schema: `sqlite3 .schema`;
-- rows: the surface_* tables only. Loaded by tests/upgrade_state.rs.
CREATE TABLE discord_conversations (guild_id TEXT NOT NULL,channel_id TEXT NOT NULL,provider TEXT NOT NULL CHECK(provider IN ('codex', 'claude')),native_session_id TEXT NOT NULL,cwd TEXT NOT NULL,created_at_ms INTEGER NOT NULL,uncertain INTEGER NOT NULL DEFAULT 0 CHECK(uncertain IN (0, 1)),PRIMARY KEY(guild_id, channel_id),UNIQUE(provider, native_session_id));
CREATE TABLE discord_native_turns (guild_id TEXT NOT NULL,channel_id TEXT NOT NULL,turn_id TEXT NOT NULL,status TEXT NOT NULL CHECK(status IN ('pending', 'complete', 'uncertain')),created_at_ms INTEGER NOT NULL,finished_at_ms INTEGER,PRIMARY KEY(guild_id, channel_id, turn_id));
CREATE INDEX idx_discord_native_turns_unfinished ON discord_native_turns(guild_id, channel_id, status);
CREATE TABLE surface_conversations (
                platform TEXT NOT NULL,
                account_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                thread_id TEXT NOT NULL DEFAULT '',
                provider TEXT NOT NULL CHECK(provider IN ('codex', 'claude')),
                native_session_id TEXT NOT NULL,
                cwd TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                uncertain INTEGER NOT NULL DEFAULT 0 CHECK(uncertain IN (0, 1)),
                PRIMARY KEY(platform, account_id, conversation_id, thread_id),
                UNIQUE(provider, native_session_id)
            );
CREATE TABLE surface_native_turns (
                platform TEXT NOT NULL,
                account_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                thread_id TEXT NOT NULL DEFAULT '',
                turn_id TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('pending', 'complete', 'uncertain')),
                created_at_ms INTEGER NOT NULL,
                finished_at_ms INTEGER,
                PRIMARY KEY(platform, account_id, conversation_id, thread_id, turn_id)
            );
CREATE INDEX idx_surface_native_turns_unfinished
            ON surface_native_turns(platform, account_id, conversation_id, thread_id, status);
CREATE TABLE surface_turn_resolutions (
                platform TEXT NOT NULL,
                account_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                thread_id TEXT NOT NULL DEFAULT '',
                turn_id TEXT NOT NULL,
                resolution TEXT NOT NULL CHECK(resolution IN ('cancelled', 'interrupted')),
                resolved_at_ms INTEGER NOT NULL,
                PRIMARY KEY(platform, account_id, conversation_id, thread_id, turn_id)
            );
CREATE TRIGGER discord_conversation_surface_insert
            AFTER INSERT ON discord_conversations BEGIN
                INSERT INTO surface_conversations
                    (platform, account_id, conversation_id, thread_id, provider, native_session_id, cwd, created_at_ms, uncertain)
                VALUES ('discord', NEW.guild_id, NEW.channel_id, '', NEW.provider, NEW.native_session_id, NEW.cwd, NEW.created_at_ms, NEW.uncertain);
            END;
CREATE TRIGGER discord_conversation_surface_uncertain
            AFTER UPDATE OF uncertain ON discord_conversations BEGIN
                UPDATE surface_conversations SET uncertain = NEW.uncertain
                WHERE platform = 'discord' AND account_id = NEW.guild_id
                    AND conversation_id = NEW.channel_id AND thread_id = '';
            END;
CREATE TRIGGER discord_turn_surface_insert
            AFTER INSERT ON discord_native_turns BEGIN
                INSERT INTO surface_native_turns
                    (platform, account_id, conversation_id, thread_id, turn_id, status, created_at_ms, finished_at_ms)
                VALUES ('discord', NEW.guild_id, NEW.channel_id, '', NEW.turn_id, NEW.status, NEW.created_at_ms, NEW.finished_at_ms);
            END;
CREATE TRIGGER discord_turn_surface_finish
            AFTER UPDATE OF status ON discord_native_turns BEGIN
                UPDATE surface_native_turns SET status = NEW.status, finished_at_ms = NEW.finished_at_ms
                WHERE platform = 'discord' AND account_id = NEW.guild_id
                    AND conversation_id = NEW.channel_id AND thread_id = '' AND turn_id = NEW.turn_id;
            END;
CREATE TABLE surface_inbound_events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            thread_id TEXT NOT NULL DEFAULT '',
            event_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            payload TEXT NOT NULL,
            occurred_at_ms INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'received'
                CHECK(status IN ('received', 'claimed', 'handled', 'dead_letter')),
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            received_at_ms INTEGER NOT NULL,
            claimed_at_ms INTEGER,
            handled_at_ms INTEGER,
            UNIQUE(platform, account_id, event_id)
        );
CREATE INDEX idx_surface_inbound_open
        ON surface_inbound_events(platform, account_id, conversation_id, thread_id, status, occurred_at_ms);
CREATE TABLE surface_outbox (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            thread_id TEXT NOT NULL DEFAULT '',
            idempotency_key TEXT NOT NULL,
            operation TEXT NOT NULL
                CHECK(operation IN ('post', 'update', 'upload', 'interaction_response')),
            target_message_id TEXT,
            payload TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'queued'
                CHECK(status IN ('queued', 'sending', 'reconcile', 'sent', 'failed', 'dead_letter', 'abandoned')),
            attempts INTEGER NOT NULL DEFAULT 0,
            max_attempts INTEGER NOT NULL CHECK(max_attempts >= 1),
            next_attempt_at_ms INTEGER NOT NULL,
            interaction_expires_at_ms INTEGER,
            fell_back INTEGER NOT NULL DEFAULT 0 CHECK(fell_back IN (0, 1)),
            provider_message_id TEXT,
            last_error TEXT,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            sent_at_ms INTEGER, last_claimed_at_ms INTEGER, reconcile_lookups INTEGER NOT NULL DEFAULT 0,
            UNIQUE(platform, account_id, idempotency_key)
        );
CREATE INDEX idx_surface_outbox_conversation
        ON surface_outbox(platform, account_id, conversation_id, thread_id, status, id);
CREATE TABLE surface_cursors (
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            thread_id TEXT NOT NULL DEFAULT '',
            last_message_id TEXT,
            last_seen_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY(platform, account_id, conversation_id, thread_id)
        );
CREATE TABLE surface_owner_bindings (
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            owner_sender_id TEXT NOT NULL,
            confirmed_at_ms INTEGER NOT NULL,
            PRIMARY KEY(platform, account_id)
        );
CREATE TABLE surface_control_conversations (
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('direct', 'channel')),
            added_at_ms INTEGER NOT NULL,
            PRIMARY KEY(platform, account_id, conversation_id),
            UNIQUE(platform, account_id, kind)
        );
CREATE TABLE surface_auth_rejections (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT,
            actor_id TEXT,
            event_kind TEXT NOT NULL,
            event_id TEXT,
            reason TEXT NOT NULL,
            occurred_at_ms INTEGER NOT NULL
        );
CREATE INDEX idx_surface_auth_rejections_account
        ON surface_auth_rejections(platform, account_id, id);
CREATE TABLE surface_listener_health (
            platform TEXT PRIMARY KEY,
            state TEXT NOT NULL CHECK(length(state) > 0),
            detail TEXT,
            recovery TEXT,
            workspaces TEXT NOT NULL DEFAULT '[]',
            dry_run INTEGER NOT NULL DEFAULT 0 CHECK(dry_run IN (0, 1)),
            last_event_at_ms INTEGER,
            last_send_at_ms INTEGER,
            state_since_ms INTEGER NOT NULL,
            heartbeat_at_ms INTEGER NOT NULL,
            pid INTEGER NOT NULL
        );
CREATE TABLE actions (id TEXT PRIMARY KEY,messageId TEXT NOT NULL,threadId TEXT,fromEmail TEXT NOT NULL,recipientEmail TEXT,subject TEXT NOT NULL,originalBody TEXT,draftBody TEXT,status TEXT NOT NULL DEFAULT 'pending',errorMessage TEXT,createdAt INTEGER NOT NULL,updatedAt INTEGER NOT NULL, retryCount INTEGER DEFAULT 0, draftId TEXT, nudgeCount INTEGER NOT NULL DEFAULT 0, lastPresetId TEXT, redraftCount INTEGER NOT NULL DEFAULT 0, toEmails TEXT, ccEmails TEXT, bccEmails TEXT, envelopeSubject TEXT, nextNudgeAtMs INTEGER, scheduledAtMs INTEGER, noticeChannelId TEXT, noticeMessageId TEXT, recomposedAtMs INTEGER, status_source TEXT, status_updated_at INTEGER, mode TEXT NOT NULL DEFAULT 'classic', generatedSource TEXT, toolCallTrace TEXT);
CREATE TABLE senders (id TEXT PRIMARY KEY,email TEXT UNIQUE NOT NULL,label TEXT,active INTEGER DEFAULT 1,createdAt INTEGER NOT NULL);
CREATE TABLE config (key TEXT PRIMARY KEY,value TEXT NOT NULL,updatedAt INTEGER NOT NULL);
CREATE TABLE gmail_accounts (id TEXT PRIMARY KEY,connectionId TEXT NOT NULL,email TEXT,label TEXT,entityId TEXT NOT NULL,active INTEGER DEFAULT 1,createdAt INTEGER NOT NULL, lastPolledAt INTEGER, lastPollOk INTEGER);
CREATE TABLE emails (messageId TEXT PRIMARY KEY,threadId TEXT,fromEmail TEXT NOT NULL,subject TEXT NOT NULL,body TEXT,receivedAt TEXT,accountEntityId TEXT,firstSeenAt INTEGER NOT NULL,triageResult TEXT,agentProcessedAt INTEGER,platform TEXT NOT NULL DEFAULT 'gmail',kind TEXT NOT NULL DEFAULT 'dm');
CREATE TABLE channel_subscriptions (id                   TEXT PRIMARY KEY,platform             TEXT NOT NULL,channel_id           TEXT NOT NULL,display_name         TEXT NOT NULL,mode                 TEXT NOT NULL,active               INTEGER NOT NULL DEFAULT 1,account_id           TEXT,last_seen_message_id TEXT,last_digest_at_ms    INTEGER,created_at_ms        INTEGER NOT NULL,updated_at_ms        INTEGER NOT NULL);
CREATE TABLE slack_workspaces (id              TEXT PRIMARY KEY,team_id         TEXT NOT NULL UNIQUE,team_name       TEXT NOT NULL,entity_id       TEXT NOT NULL,connection_id   TEXT NOT NULL,user_id         TEXT NOT NULL,active          INTEGER NOT NULL DEFAULT 1,created_at_ms   INTEGER NOT NULL);
CREATE TABLE drive_accounts (id            TEXT PRIMARY KEY,connection_id TEXT NOT NULL,entity_id     TEXT NOT NULL,email         TEXT,label         TEXT,active        INTEGER NOT NULL DEFAULT 1,created_at_ms INTEGER NOT NULL);
CREATE TABLE drive_sync_state (entity_id     TEXT PRIMARY KEY,page_token    TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);
CREATE INDEX idx_actions_scheduled ON actions(status, scheduledAtMs);
CREATE INDEX idx_channel_subs_active_mode ON channel_subscriptions(active, mode);
CREATE INDEX idx_slack_workspaces_active ON slack_workspaces(active);
CREATE TABLE tone_profiles (id                       TEXT PRIMARY KEY,scope_kind               TEXT NOT NULL CHECK (scope_kind IN ('global','domain','recipient')),scope_value              TEXT NOT NULL,account_entity_id        TEXT,summary                  TEXT NOT NULL,exemplar_ids             TEXT NOT NULL DEFAULT '[]',sample_count             INTEGER NOT NULL DEFAULT 0,sample_count_at_refresh  INTEGER NOT NULL DEFAULT 0,last_refreshed_at        INTEGER NOT NULL,created_at_ms            INTEGER NOT NULL,updated_at_ms            INTEGER NOT NULL,UNIQUE(scope_kind, scope_value, account_entity_id));
CREATE INDEX idx_tone_profiles_scope ON tone_profiles(scope_kind, scope_value);
CREATE TABLE tone_examples (id                  TEXT PRIMARY KEY,source              TEXT NOT NULL CHECK (source IN ('sent_backfill','user_edit','approved_clean')),action_id           TEXT,message_id          TEXT,account_entity_id   TEXT NOT NULL,recipient_email     TEXT NOT NULL,recipient_domain    TEXT NOT NULL,subject             TEXT,body                TEXT NOT NULL,body_chars          INTEGER NOT NULL,sent_at_ms          INTEGER NOT NULL,ingested_at_ms      INTEGER NOT NULL,weight              REAL NOT NULL DEFAULT 1.0,FOREIGN KEY (action_id) REFERENCES actions(id) ON DELETE SET NULL);
CREATE INDEX idx_tone_examples_recipient ON tone_examples(recipient_email, sent_at_ms DESC);
CREATE INDEX idx_tone_examples_domain ON tone_examples(recipient_domain, sent_at_ms DESC);
CREATE INDEX idx_tone_examples_account_recent ON tone_examples(account_entity_id, sent_at_ms DESC);
CREATE TABLE draft_revisions (id                 TEXT PRIMARY KEY,actionId           TEXT NOT NULL REFERENCES actions(id) ON DELETE CASCADE,iteration          INTEGER NOT NULL,draftBody          TEXT NOT NULL,feedbackText       TEXT,presetId           TEXT,outcome            TEXT NOT NULL,modelId            TEXT NOT NULL,promptTokens       INTEGER,completionTokens   INTEGER,createdAt          INTEGER NOT NULL,UNIQUE(actionId, iteration));
CREATE INDEX idx_draft_revisions_outcome ON draft_revisions(outcome, createdAt);
CREATE TABLE rate_events (id              TEXT PRIMARY KEY,platform        TEXT NOT NULL,action_kind     TEXT NOT NULL,account_id      TEXT NOT NULL,occurred_at_ms  INTEGER NOT NULL,status          TEXT NOT NULL,cause           TEXT NOT NULL,target_id       TEXT,meta_json       TEXT);
CREATE INDEX idx_rate_events_window ON rate_events(platform, action_kind, account_id, occurred_at_ms);
CREATE INDEX idx_rate_events_audit ON rate_events(platform, occurred_at_ms);
CREATE TABLE rate_halts (platform               TEXT PRIMARY KEY,paused_until_ms        INTEGER NOT NULL,reason                 TEXT NOT NULL,triggered_by_event_id  TEXT,created_at_ms          INTEGER NOT NULL,acknowledged_at_ms     INTEGER);
CREATE TABLE rate_warmup (platform              TEXT NOT NULL,account_id            TEXT NOT NULL,warmup_started_at_ms  INTEGER NOT NULL,PRIMARY KEY (platform, account_id));
CREATE TABLE telegram_bots (id              TEXT PRIMARY KEY,bot_id          INTEGER NOT NULL UNIQUE,bot_username    TEXT NOT NULL,owner_chat_id   INTEGER NOT NULL,last_update_id  INTEGER NOT NULL DEFAULT 0,active          INTEGER NOT NULL DEFAULT 1,created_at_ms   INTEGER NOT NULL);
CREATE INDEX idx_telegram_bots_active ON telegram_bots(active);
CREATE TABLE whatsapp_devices (id                TEXT PRIMARY KEY,phone             TEXT NOT NULL UNIQUE,device_jid        TEXT NOT NULL,user_jid          TEXT NOT NULL,paired_at_ms      INTEGER NOT NULL,last_event_at_ms  INTEGER NOT NULL DEFAULT 0,session_status    TEXT NOT NULL DEFAULT 'paired',active            INTEGER NOT NULL DEFAULT 1,created_at_ms     INTEGER NOT NULL);
CREATE INDEX idx_whatsapp_devices_active ON whatsapp_devices(active);
CREATE TABLE whatsapp_outbound_allowlist (chat_jid       TEXT PRIMARY KEY,enabled_at_ms  INTEGER NOT NULL);
CREATE TABLE whatsapp_inbound_allowlist (chat_jid       TEXT PRIMARY KEY,enabled_at_ms  INTEGER NOT NULL);
CREATE TABLE user_loops (id             TEXT PRIMARY KEY,owner          TEXT NOT NULL,channel        TEXT NOT NULL,channel_ref    TEXT NOT NULL,interval_secs  INTEGER NOT NULL,prompt         TEXT NOT NULL,status         TEXT NOT NULL DEFAULT 'active',last_run_ms    INTEGER,last_status    TEXT,fail_count     INTEGER NOT NULL DEFAULT 0,created_at_ms  INTEGER NOT NULL,updated_at_ms  INTEGER NOT NULL,expires_at_ms  INTEGER, cron_expr TEXT, tz TEXT, model_profile TEXT, nag_until_ack INTEGER NOT NULL DEFAULT 0, nag_cycle_ms INTEGER);
CREATE INDEX idx_user_loops_owner_status ON user_loops(owner, status);
CREATE TABLE gmail_fetch_cooldowns (entityId TEXT PRIMARY KEY,retryAfterMs INTEGER NOT NULL,logId TEXT,observedAtMs INTEGER NOT NULL);
CREATE TABLE proactive_signals (id                     TEXT PRIMARY KEY,kind                   TEXT NOT NULL,person_slug            TEXT,urgency                TEXT NOT NULL,headline               TEXT NOT NULL,detail                 TEXT NOT NULL,suggested_action_json  TEXT,status                 TEXT NOT NULL,snooze_until_ms        INTEGER,dedup_key              TEXT NOT NULL,created_at_ms          INTEGER NOT NULL,dispatched_at_ms       INTEGER);
CREATE INDEX idx_proactive_signals_status_created ON proactive_signals(status, created_at_ms);
CREATE INDEX idx_proactive_signals_dedup_recent ON proactive_signals(dedup_key, created_at_ms);
CREATE INDEX idx_proactive_signals_person ON proactive_signals(person_slug);
CREATE TABLE proactive_scan_runs (scan_id         TEXT PRIMARY KEY,last_run_at_ms  INTEGER NOT NULL);
CREATE TABLE heartbeat_runs (id              INTEGER PRIMARY KEY AUTOINCREMENT,started_at_ms   INTEGER NOT NULL,finished_at_ms  INTEGER,status          TEXT NOT NULL,reason          TEXT,message         TEXT,message_hash    TEXT,duration_ms     INTEGER);
CREATE INDEX idx_heartbeat_runs_started ON heartbeat_runs(started_at_ms);
CREATE TABLE heartbeat_lease (id             INTEGER PRIMARY KEY CHECK (id = 1),holder         TEXT NOT NULL,expires_at_ms  INTEGER NOT NULL);
CREATE TABLE twitter_query_ids (operation      TEXT PRIMARY KEY,query_id       TEXT NOT NULL,last_seen_at   INTEGER NOT NULL);
CREATE TABLE twitter_post_log (id              TEXT PRIMARY KEY,kind            TEXT NOT NULL,reply_to        TEXT,status          TEXT NOT NULL,tweet_id        TEXT,occurred_at_ms  INTEGER NOT NULL,meta_json       TEXT);
CREATE INDEX idx_twitter_post_log_window ON twitter_post_log(occurred_at_ms);
CREATE TABLE linkedin_action_log (id              TEXT PRIMARY KEY,action_kind     TEXT NOT NULL,target_urn      TEXT,status          TEXT NOT NULL,occurred_at_ms  INTEGER NOT NULL,meta_json       TEXT);
CREATE INDEX idx_linkedin_action_log_window ON linkedin_action_log(action_kind, occurred_at_ms);
CREATE TABLE scheduled_posts (id              TEXT PRIMARY KEY,platform        TEXT NOT NULL,body            TEXT NOT NULL,media_paths     TEXT,fire_at_ms      INTEGER NOT NULL,status          TEXT NOT NULL,approval_msg    TEXT,posted_at_ms    INTEGER,external_id     TEXT,thread_parent   TEXT REFERENCES scheduled_posts(id) ON DELETE SET NULL,created_at_ms   INTEGER NOT NULL, socialapi_account_id TEXT);
CREATE INDEX idx_scheduled_posts_fire ON scheduled_posts(status, fire_at_ms);
CREATE TABLE own_posts (id            TEXT PRIMARY KEY,platform      TEXT NOT NULL,external_id   TEXT NOT NULL,posted_at_ms  INTEGER NOT NULL,poll_until_ms INTEGER NOT NULL,last_polled_ms INTEGER,created_at_ms INTEGER NOT NULL,UNIQUE (platform, external_id));
CREATE INDEX idx_own_posts_poll ON own_posts(platform, poll_until_ms);
CREATE TABLE seen_comments (id            TEXT PRIMARY KEY,own_post_id   TEXT NOT NULL REFERENCES own_posts(id) ON DELETE CASCADE,external_id   TEXT NOT NULL,author_handle TEXT,body          TEXT,triage_id     TEXT,created_at_ms INTEGER NOT NULL,UNIQUE (own_post_id, external_id));
CREATE TABLE friend_watchlist (id            TEXT PRIMARY KEY,platform      TEXT NOT NULL,handle        TEXT NOT NULL,wiki_slug     TEXT,engagement    TEXT NOT NULL DEFAULT 'medium',added_at_ms   INTEGER NOT NULL,paused_until_ms INTEGER,UNIQUE (platform, handle));
CREATE TABLE friend_posts_seen (id            TEXT PRIMARY KEY,watchlist_id  TEXT NOT NULL REFERENCES friend_watchlist(id) ON DELETE CASCADE,external_id   TEXT NOT NULL,posted_at_ms  INTEGER NOT NULL,triage_id     TEXT,UNIQUE (watchlist_id, external_id));
CREATE TABLE connection_requests (id              TEXT PRIMARY KEY,platform        TEXT NOT NULL,external_id     TEXT NOT NULL,requester_name  TEXT,requester_url   TEXT,message         TEXT,decision        TEXT NOT NULL DEFAULT 'pending',decided_at_ms   INTEGER,triage_id       TEXT,created_at_ms   INTEGER NOT NULL,UNIQUE (platform, external_id));
CREATE INDEX idx_connection_requests_decision ON connection_requests(decision, created_at_ms);
CREATE TABLE warm_touch_state (wiki_slug            TEXT PRIMARY KEY,last_interaction_ms  INTEGER,last_nudged_ms       INTEGER,snoozed_until_ms     INTEGER,cadence_days         INTEGER);
CREATE INDEX idx_drive_accounts_active ON drive_accounts(active);
CREATE TABLE journal_sync_state (owner_id      TEXT PRIMARY KEY,last_sync_ms  INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL);
CREATE TABLE journal_sync_cursor (owner_id      TEXT PRIMARY KEY,last_sync_ms  INTEGER,started_at_ms INTEGER NOT NULL,next_token    TEXT,updated_at_ms INTEGER NOT NULL);
CREATE TABLE journal_ingested (owner_id       TEXT NOT NULL,entry_id       TEXT NOT NULL,version        INTEGER NOT NULL,ingested_at_ms INTEGER NOT NULL,PRIMARY KEY (owner_id, entry_id, version));
CREATE TABLE imessage_sync_state (conversation  TEXT PRIMARY KEY,entries_seen  INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL);
CREATE TABLE voice_capture_state (bot_key         TEXT PRIMARY KEY,last_update_id  INTEGER NOT NULL DEFAULT 0,updated_at_ms   INTEGER NOT NULL);
CREATE TABLE calendar_alerts (key          TEXT PRIMARY KEY,fingerprint  TEXT NOT NULL,sent_at_ms   INTEGER NOT NULL);
CREATE TABLE outbound_state (entity_id              TEXT PRIMARY KEY,last_seen_sent_at_ms   INTEGER NOT NULL DEFAULT 0,updated_at_ms          INTEGER NOT NULL);
CREATE TABLE outbound_thread_log (entity_id    TEXT NOT NULL,message_id   TEXT NOT NULL,thread_id    TEXT,sent_at_ms   INTEGER NOT NULL,recorded_at_ms INTEGER NOT NULL,PRIMARY KEY (entity_id, message_id));
CREATE INDEX idx_outbound_thread_log_thread_sent ON outbound_thread_log (thread_id, sent_at_ms);
CREATE TABLE self_sent_messages (message_id   TEXT PRIMARY KEY,thread_id    TEXT,entity_id    TEXT,action_id    TEXT,sent_at_ms   INTEGER NOT NULL);
CREATE INDEX idx_self_sent_messages_thread_sent ON self_sent_messages (thread_id, sent_at_ms);
CREATE TABLE proactive_user_actions (id            TEXT PRIMARY KEY,action        TEXT NOT NULL,scope         TEXT NOT NULL,created_at_ms INTEGER NOT NULL,expires_at_ms INTEGER);
CREATE TABLE pwa_subscriptions (id            TEXT PRIMARY KEY,endpoint      TEXT NOT NULL UNIQUE,p256dh        TEXT NOT NULL,auth          TEXT NOT NULL,created_at_ms INTEGER NOT NULL);
CREATE TABLE linkedin_connection_sync (account_id          TEXT PRIMARY KEY,last_full_sync_ms   INTEGER,last_delta_sync_ms  INTEGER,cursor_start        INTEGER NOT NULL DEFAULT 0,last_synced_count   INTEGER NOT NULL DEFAULT 0,updated_at_ms       INTEGER NOT NULL);
CREATE TABLE contacts_sync_state (backend       TEXT NOT NULL,account_id    TEXT NOT NULL,sync_token    TEXT,updated_at_ms INTEGER NOT NULL,PRIMARY KEY (backend, account_id));
CREATE TABLE identity_phone (phone         TEXT PRIMARY KEY,person_slug   TEXT NOT NULL,display_name  TEXT,source        TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);
CREATE INDEX idx_proactive_user_actions_lookup ON proactive_user_actions(action, scope, expires_at_ms);
CREATE TABLE detected_asks (id              TEXT PRIMARY KEY,message_id      TEXT NOT NULL,platform        TEXT NOT NULL,ask_text        TEXT NOT NULL,resolver_kind   TEXT NOT NULL,auto_fillable   INTEGER NOT NULL DEFAULT 0,confidence      REAL,raw_json        TEXT,detected_at_ms  INTEGER NOT NULL);
CREATE INDEX idx_detected_asks_msg ON detected_asks(message_id);
CREATE INDEX idx_detected_asks_recent ON detected_asks(detected_at_ms);
CREATE INDEX idx_identity_phone_slug ON identity_phone(person_slug);
CREATE TABLE agent_repos (id                 TEXT PRIMARY KEY,full_name          TEXT NOT NULL UNIQUE COLLATE NOCASE,base_branch        TEXT NOT NULL DEFAULT 'main',build_cmd          TEXT NOT NULL DEFAULT '',blast_radius_extra TEXT NOT NULL DEFAULT '',max_diff_lines     INTEGER NOT NULL DEFAULT 600,enabled            INTEGER NOT NULL DEFAULT 1,created_at_ms      INTEGER NOT NULL,updated_at_ms      INTEGER NOT NULL);
CREATE INDEX idx_agent_repos_enabled ON agent_repos(enabled);
CREATE TABLE agent_pr_runs (id             TEXT PRIMARY KEY,repo_full_name TEXT NOT NULL,issue_number   INTEGER NOT NULL,branch         TEXT NOT NULL,summary        TEXT NOT NULL DEFAULT '',diff_lines     INTEGER NOT NULL DEFAULT 0,status         TEXT NOT NULL,pr_url         TEXT,error          TEXT,created_at_ms  INTEGER NOT NULL,updated_at_ms  INTEGER NOT NULL);
CREATE INDEX idx_agent_pr_runs_repo ON agent_pr_runs(repo_full_name, created_at_ms);
CREATE INDEX idx_agent_pr_runs_status ON agent_pr_runs(status);
CREATE TABLE memory (id            TEXT PRIMARY KEY,created_at_ms INTEGER NOT NULL,surface       TEXT NOT NULL,subject       TEXT NOT NULL,body          TEXT NOT NULL,tags          TEXT NOT NULL DEFAULT '');
CREATE VIRTUAL TABLE memory_fts USING fts5(subject, body, tags, surface UNINDEXED, content='memory', content_rowid='rowid', tokenize='porter unicode61')
/* memory_fts(subject,body,tags,surface) */;
CREATE TABLE IF NOT EXISTS 'memory_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
CREATE TABLE IF NOT EXISTS 'memory_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS 'memory_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE IF NOT EXISTS 'memory_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE TRIGGER memory_ai AFTER INSERT ON memory BEGIN INSERT INTO memory_fts(rowid, subject, body, tags, surface) VALUES (new.rowid, new.subject, new.body, new.tags, new.surface); END;
CREATE TRIGGER memory_ad AFTER DELETE ON memory BEGIN INSERT INTO memory_fts(memory_fts, rowid, subject, body, tags, surface) VALUES('delete', old.rowid, old.subject, old.body, old.tags, old.surface); END;
CREATE TRIGGER memory_au AFTER UPDATE ON memory BEGIN INSERT INTO memory_fts(memory_fts, rowid, subject, body, tags, surface) VALUES('delete', old.rowid, old.subject, old.body, old.tags, old.surface); INSERT INTO memory_fts(rowid, subject, body, tags, surface) VALUES (new.rowid, new.subject, new.body, new.tags, new.surface); END;
CREATE INDEX idx_memory_created ON memory(created_at_ms DESC);
CREATE INDEX idx_memory_surface_created ON memory(surface, created_at_ms DESC);
CREATE TABLE socialapi_accounts (id             TEXT PRIMARY KEY,brand_id       TEXT,platform       TEXT NOT NULL,display_name   TEXT,account_handle TEXT,active         INTEGER NOT NULL DEFAULT 1,created_at_ms  INTEGER NOT NULL,updated_at_ms  INTEGER NOT NULL);
CREATE INDEX idx_socialapi_accounts_active ON socialapi_accounts(active);
CREATE TABLE socialapi_seen_comments (post_id       TEXT NOT NULL,comment_id    TEXT NOT NULL,author        TEXT,text          TEXT,seen_at_ms    INTEGER NOT NULL,PRIMARY KEY (post_id, comment_id));
CREATE TABLE socialapi_seen_dms (conversation_id TEXT NOT NULL,message_id      TEXT NOT NULL,author          TEXT,text            TEXT,seen_at_ms      INTEGER NOT NULL,PRIMARY KEY (conversation_id, message_id));
CREATE TABLE socialapi_webhook_events (id             TEXT PRIMARY KEY,kind           TEXT NOT NULL CHECK (kind IN ('dm', 'comment')),account_id     TEXT,payload_json   TEXT NOT NULL,received_at_ms INTEGER NOT NULL,processed      INTEGER NOT NULL DEFAULT 0);
CREATE INDEX idx_socialapi_webhook_events_unprocessed ON socialapi_webhook_events(processed, received_at_ms);
CREATE TABLE channel_drafts (id                  TEXT PRIMARY KEY,target_channel      TEXT NOT NULL,payload_json        TEXT NOT NULL,status              TEXT NOT NULL DEFAULT 'pending',note                TEXT,created_at_ms       INTEGER NOT NULL,updated_at_ms       INTEGER NOT NULL,approved_at_ms      INTEGER,published_at_ms     INTEGER,discarded_at_ms     INTEGER,publish_result_json TEXT,error_message       TEXT);
CREATE INDEX idx_channel_drafts_status ON channel_drafts(status, created_at_ms DESC);
CREATE INDEX idx_channel_drafts_target ON channel_drafts(target_channel, status);
CREATE TABLE research_seen (arxiv_id  TEXT PRIMARY KEY,seen_at   INTEGER NOT NULL);
CREATE INDEX idx_actions_status ON actions(status);
CREATE INDEX idx_actions_created ON actions(createdAt);
CREATE INDEX idx_actions_messageId ON actions(messageId);
CREATE INDEX idx_gmail_accounts_active ON gmail_accounts(active);
CREATE INDEX idx_emails_triage ON emails(triageResult);
CREATE INDEX idx_emails_seen ON emails(firstSeenAt);
CREATE INDEX idx_emails_platform ON emails(platform);
CREATE TABLE message_index (message_id         TEXT PRIMARY KEY,platform           TEXT NOT NULL,conv_kind          TEXT NOT NULL,conversation_id    TEXT NOT NULL,conversation_title TEXT,container          TEXT,sender_handle      TEXT NOT NULL,sender_label       TEXT,counterpart_handle TEXT,from_me            INTEGER NOT NULL,ts_ms              INTEGER NOT NULL,ts_fallback        INTEGER NOT NULL DEFAULT 0,has_attachment     INTEGER NOT NULL,extractor_version  INTEGER NOT NULL);
CREATE INDEX idx_mi_conv ON message_index(conversation_id, ts_ms);
CREATE INDEX idx_mi_sender ON message_index(sender_handle, ts_ms);
CREATE INDEX idx_mi_counterpart ON message_index(counterpart_handle, ts_ms);
CREATE INDEX idx_mi_plat ON message_index(platform, conv_kind, ts_ms);
CREATE INDEX idx_mi_ts ON message_index(ts_ms);
CREATE TABLE message_index_queue (seq        INTEGER PRIMARY KEY AUTOINCREMENT,message_id TEXT NOT NULL UNIQUE);
CREATE TRIGGER trg_emails_message_index_insert AFTER INSERT ON emails BEGIN INSERT OR REPLACE INTO message_index_queue(message_id) VALUES (NEW.messageId); END;
CREATE TRIGGER trg_emails_message_index_update AFTER UPDATE OF messageId, threadId, fromEmail, subject, body, receivedAt, accountEntityId, platform, kind ON emails WHEN OLD.threadId IS NOT NEW.threadId OR OLD.fromEmail IS NOT NEW.fromEmail OR OLD.subject IS NOT NEW.subject OR OLD.body IS NOT NEW.body OR OLD.receivedAt IS NOT NEW.receivedAt OR OLD.accountEntityId IS NOT NEW.accountEntityId OR OLD.platform IS NOT NEW.platform OR OLD.kind IS NOT NEW.kind BEGIN INSERT OR REPLACE INTO message_index_queue(message_id) VALUES (NEW.messageId); END;
CREATE TRIGGER trg_emails_message_index_delete AFTER DELETE ON emails BEGIN INSERT OR REPLACE INTO message_index_queue(message_id) VALUES (OLD.messageId); END;
CREATE TABLE message_people (handle     TEXT PRIMARY KEY,person_key TEXT NOT NULL);
CREATE INDEX idx_message_people_person ON message_people(person_key);
CREATE TABLE message_person_names (person_key TEXT NOT NULL,name       TEXT NOT NULL,PRIMARY KEY (person_key, name));
CREATE INDEX idx_message_person_names_name ON message_person_names(name);
CREATE TABLE message_people_meta (key   TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE VIEW conversation_people AS SELECT h.conversation_id, h.handle, mp.person_key FROM ( SELECT conversation_id, sender_handle AS handle FROM message_index WHERE from_me = 0 UNION SELECT conversation_id, counterpart_handle FROM message_index WHERE counterpart_handle IS NOT NULL ) h LEFT JOIN message_people mp ON mp.handle = h.handle
/* conversation_people(conversation_id,handle,person_key) */;
CREATE VIRTUAL TABLE message_fts USING fts5(title, subject, body, tokenize = 'porter unicode61 remove_diacritics 2')
/* message_fts(title,subject,body) */;
CREATE TABLE IF NOT EXISTS 'message_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
CREATE TABLE IF NOT EXISTS 'message_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS 'message_fts_content'(id INTEGER PRIMARY KEY, c0, c1, c2);
CREATE TABLE IF NOT EXISTS 'message_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE IF NOT EXISTS 'message_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE TABLE imessage_outbox (id              INTEGER PRIMARY KEY AUTOINCREMENT,action_id       TEXT NOT NULL UNIQUE,target          TEXT NOT NULL,target_kind     TEXT NOT NULL CHECK(target_kind IN ('handle', 'chat_guid')),service         TEXT NOT NULL,body            TEXT NOT NULL,status          TEXT NOT NULL DEFAULT 'queued' CHECK(status IN ('queued', 'claimed', 'sent', 'failed', 'unknown')),created_at_ms   INTEGER NOT NULL,claimed_at_ms   INTEGER,completed_at_ms INTEGER,error_code      INTEGER,error_detail    TEXT,message_guid    TEXT,notified_at_ms  INTEGER);
CREATE INDEX idx_imessage_outbox_status ON imessage_outbox(status, id);
CREATE INDEX idx_imessage_outbox_target ON imessage_outbox(target, status);
CREATE TABLE imessage_outbound_allowlist (identifier    TEXT PRIMARY KEY,enabled_at_ms INTEGER NOT NULL);
CREATE TABLE imessage_inbound_allowlist (identifier    TEXT PRIMARY KEY,enabled_at_ms INTEGER NOT NULL);
BEGIN;
INSERT INTO surface_inbound_events VALUES(1,'slack','team:T00000001','D00000001','','D00000001:1700000000.000100','message','{"type":"events_api","envelope_id":"env-fixture-1","accepts_response_payload":false,"retry_attempt":null,"retry_reason":null,"payload":{"type":"event_callback","team_id":"T00000001","api_app_id":"A00000001","event_id":"EvFixture1","event_time":1700000000,"event":{"type":"message","channel":"D00000001","channel_type":"im","user":"U00000001","text":"synthetic pending question","ts":"1700000000.000100"}}}',1700000000000,'received',0,NULL,1700000000000,NULL,NULL);
INSERT INTO surface_outbox VALUES(1,'slack','team:T00000001','D00000001','','turn:D00000001:1699999999.000100:text:0','post',NULL,'{"text":"synthetic queued answer","part":1,"parts":1}','reconcile',1,5,1700000000000,NULL,0,NULL,NULL,1700000000000,1700000000000,NULL,1700000000000,0);
INSERT INTO surface_outbox VALUES(2,'slack','team:T00000001','C00000001','1699999998.000100','turn:C00000001:1699999998.000100:text:0','post',NULL,'{"text":"synthetic dead letter","part":1,"parts":1}','dead_letter',1,5,1700000000000,NULL,0,NULL,'channel_not_found',1700000000000,1700000000000,NULL,1700000000000,0);
INSERT INTO surface_outbox VALUES(3,'slack','team:T00000001','D00000001','','turn:D00000001:1700000000.000050:text:0','post',NULL,'{"text":"synthetic second queued answer","part":1,"parts":1}','queued',0,5,1700000000000,NULL,0,NULL,NULL,1700000000000,1700000000000,NULL,NULL,0);
INSERT INTO surface_owner_bindings VALUES('slack','team:T00000001','U00000001',1700000000000);
INSERT INTO surface_control_conversations VALUES('slack','team:T00000001','D00000001','direct',1700000000000);
INSERT INTO surface_control_conversations VALUES('slack','team:T00000001','C00000001','channel',1700000000000);
INSERT INTO surface_listener_health VALUES('slack','connected',NULL,NULL,'["T00000001"]',0,1699999995000,1699999996000,1699999940000,1700000000000,4242);
COMMIT;
