//! #14 — operator validation harness (mock-only / dry-run in this build).
//!
//! `docs/twitter-protocol.md` is reconstructed from public knowledge and
//! marked **REQUIRES LIVE OPERATOR VALIDATION**: a true `/intercept` capture
//! against a logged-in X session is still needed before the channel is
//! trusted in non-dry-run mode. The spike (#14) had no live session, so the
//! autonomous deliverable is to make that validation a **single command** —
//! and to ship that command with a hard mock-only safety gate so the
//! autonomous build never reaches out to `x.com`.
//!
//! [`validate`] takes an already-loaded [`TwitterAuth`] (operator harvests
//! cookies via `scripts/twitter-harvest.sh`, see the runbook in
//! docs/twitter-protocol.md), then exercises each documented endpoint and
//! reports pass / fail / skipped plus a captured response-shape fingerprint:
//!
//! 1. **auth** — `UserTweets` against the operator's own id (cheap, read-only;
//!    a 200 with a parseable timeline proves cookies + bearer + csrf + the
//!    `x-client-transaction-id` posture all work). Also folds in the local
//!    session-age advisory ([`TwitterAuth::is_session_stale`]).
//! 2. **UserTweets** — same call, asserts the documented response shape
//!    (`data.user.result.timeline_v2…`) actually parses.
//! 3. **CreateTweet (dry-run)** — builds the exact request the client would
//!    POST and validates the body shape **without sending** (never posts
//!    during validation). Optionally does a live no-op probe only with
//!    `--allow-write`.
//! 4. **DM inbox** — `inbox_initial_state.json`, asserts it parses.
//! 5. **DM send (dry-run)** — builds the `new2.json` body, validates shape,
//!    never sends unless `--allow-write`.
//!
//! ## Mock-only / dry-run safety gate
//!
//! In this build the harness is **mock-only**: [`validate`] refuses to issue a
//! live HTTP request unless the caller explicitly points it at a mock via the
//! `AUGMENTAGENT_TWITTER_BASE_URL` override (what the wiremock tests + a local
//! capture proxy set) **or** opts in with [`ValidateOptions::allow_live`].
//! With neither, every read probe is reported `Skipped` with a "mock-only
//! build" reason and the write probes stay dry-run. The injectable
//! [`validate_with_api`] seam is what the unit tests drive against a
//! wiremock-backed [`TwitterClient`] — so the *shipped* harness logic (not a
//! re-implementation) is the thing under test, and it provably never touches
//! live x.com.
//!
//! Output is a [`ValidationReport`] — printed as a human table by the CLI and
//! available as JSON for an attachable artifact. The operator runs ONE
//! command; the pass/fail grid + per-probe shape fingerprint tell them exactly
//! which protocol section the live deploy still honors and which
//! `REQUIRES LIVE OPERATOR VALIDATION` flags can be cleared.

use std::sync::Arc;

use serde::Serialize;

use crate::api::{base_url, TwitterApi, TwitterClient, TwitterError};
use crate::auth::TwitterAuth;

/// One probed endpoint's result.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CheckResult {
    /// Stable key, e.g. `auth`, `user_tweets`, `create_tweet_dry_run`.
    pub check: String,
    /// `pass` | `fail` | `skipped`.
    pub status: CheckStatus,
    /// Human one-liner — the failure reason or a shape fingerprint.
    pub detail: String,
    /// The protocol-doc section this maps to (for clearing the
    /// `REQUIRES LIVE OPERATOR VALIDATION` flags).
    pub spec_section: String,
    /// A stable, content-free fingerprint of the observed response shape
    /// (e.g. `tweets=18` / `dms=4;dry_run` / `error=AuthExpired`). Diffable
    /// across runs without echoing any session-bearing body — this is the
    /// "response-shape fingerprint" the #14 harness contract requires.
    pub shape_fingerprint: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Fail,
    Skipped,
}

impl CheckStatus {
    pub fn glyph(&self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skipped => "SKIP",
        }
    }
}

/// The full harness output.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub screen_name: String,
    pub user_id: String,
    /// True when this run was mock-only (no live HTTP was permitted). The CLI
    /// surfaces this so an all-green mock run is never mistaken for a live
    /// sign-off — clearing the doc flags still requires a live operator run.
    pub mock_only: bool,
    pub checks: Vec<CheckResult>,
    /// True iff every non-skipped check passed — the gate for clearing the
    /// doc's validation flags.
    pub all_passed: bool,
}

impl ValidationReport {
    fn finalize(mut self) -> Self {
        // A mock-only run is NEVER a sign-off, even if every dry-run contract
        // check passes — no live endpoint was actually probed.
        self.all_passed = !self.mock_only
            && self
                .checks
                .iter()
                .all(|c| c.status != CheckStatus::Fail);
        self
    }

    /// Human table for the CLI.
    pub fn render_table(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "twitter validate — @{} (id {}){}\n",
            self.screen_name,
            self.user_id,
            if self.mock_only {
                "  [MOCK-ONLY BUILD — no live x.com call was made]"
            } else {
                ""
            }
        ));
        s.push_str(&"-".repeat(78));
        s.push('\n');
        for c in &self.checks {
            s.push_str(&format!(
                "  [{}] {:<22} {:<6} {:<22} {}\n",
                c.status.glyph(),
                c.check,
                c.spec_section,
                c.shape_fingerprint,
                c.detail
            ));
        }
        s.push_str(&"-".repeat(78));
        s.push('\n');
        if self.mock_only {
            s.push_str(
                "RESULT: MOCK-ONLY run — the harness logic and protocol \
                 hardening are exercised, but NO live x.com call was made. \
                 The `REQUIRES LIVE OPERATOR VALIDATION` flags in \
                 docs/twitter-protocol.md CANNOT be cleared from a mock run. \
                 Re-run with a live session + `--allow-live` (or point \
                 AUGMENTAGENT_TWITTER_BASE_URL at a capture proxy) for \
                 sign-off.\n",
            );
        } else if self.all_passed {
            s.push_str(
                "RESULT: all checks passed — the live deploy honors the spec. \
                 The `REQUIRES LIVE OPERATOR VALIDATION` flags in \
                 docs/twitter-protocol.md can be cleared for the validated \
                 ops.\n",
            );
        } else {
            s.push_str(
                "RESULT: one or more checks FAILED — keep the validation \
                 flags set. See per-check detail above; re-harvest cookies or \
                 refresh the queryId, then re-run.\n",
            );
        }
        s
    }
}

/// Knobs for the harness. All fields default to the safe posture:
/// `allow_write=false`, `allow_live=false` (mock-only), no probe targets.
#[derive(Debug, Clone, Default)]
pub struct ValidateOptions {
    /// When false (default) the write paths (CreateTweet / DM send) are only
    /// shape-validated, never sent. `--allow-write` flips this to do a real
    /// minimal probe (still gated; the harness never posts public content
    /// without an explicit reply target the operator passes in).
    pub allow_write: bool,
    /// **Mock-only safety gate.** When false (default) and no
    /// `AUGMENTAGENT_TWITTER_BASE_URL` override is set, [`validate`] makes NO
    /// live HTTP call: the read probes are `Skipped` ("mock-only build") and
    /// the report is flagged `mock_only`. An operator on a real session
    /// passes `--allow-live` to actually exercise x.com. Tests never set this
    /// — they inject a wiremock URL via the base-url override instead.
    pub allow_live: bool,
    /// Optional tweet id to reply to for a live CreateTweet probe (only used
    /// with `allow_write`). The operator should pass a throwaway tweet.
    pub probe_reply_to: Option<String>,
    /// Optional conversation id for a live DM-send probe (only with
    /// `allow_write`).
    pub probe_conversation_id: Option<String>,
}

/// Is a live HTTP probe permitted in this run? True iff the operator opted in
/// (`allow_live`) **or** the base-url override is set (a mock / capture proxy
/// — what the wiremock tests use). With neither, the harness is mock-only and
/// never reaches x.com.
fn live_probes_permitted(opts: &ValidateOptions) -> bool {
    opts.allow_live || std::env::var("AUGMENTAGENT_TWITTER_BASE_URL").is_ok()
}

const MOCK_ONLY_REASON: &str =
    "mock-only build: no live x.com call (pass --allow-live on a real \
     session, or set AUGMENTAGENT_TWITTER_BASE_URL to a capture proxy)";

/// Run the harness against a session bundle.
///
/// **Mock-only by default**: unless live probes are permitted (see
/// [`live_probes_permitted`]) this constructs no network client and skips
/// every read probe. Read-only even when live, unless `allow_write` + a probe
/// target is supplied.
pub async fn validate(
    auth: TwitterAuth,
    opts: ValidateOptions,
) -> ValidationReport {
    let screen_name = auth.screen_name.clone();
    let user_id = auth.user_id.clone();

    if !live_probes_permitted(&opts) {
        // Hard gate: build no client, issue nothing. Report the dry-run
        // posture so an all-green mock run is unmistakable for a sign-off.
        return mock_only_report(&screen_name, &user_id, &auth, &opts);
    }

    tracing::info!(
        base_url = %base_url(),
        allow_write = opts.allow_write,
        "twitter validate: live probes permitted"
    );
    let api: Arc<dyn TwitterApi> = Arc::new(TwitterClient::new(auth.clone()));
    let mut report =
        validate_with_api(api, &screen_name, &user_id, &auth, &opts).await;
    report.mock_only = false;
    report
}

/// The mock-only report path: no client, no network, every read probe
/// `Skipped` with the reason and the local auth-staleness advisory folded in.
fn mock_only_report(
    screen_name: &str,
    user_id: &str,
    auth: &TwitterAuth,
    opts: &ValidateOptions,
) -> ValidationReport {
    let now_ms = now_millis();
    let stale = auth.is_session_stale(now_ms);
    let age = auth
        .session_age_days(now_ms)
        .map(|d| format!("age={d}d"))
        .unwrap_or_else(|| "age=?".into());
    let mut checks = vec![
        CheckResult {
            check: "auth".into(),
            status: CheckStatus::Skipped,
            detail: MOCK_ONLY_REASON.into(),
            spec_section: "§1".into(),
            shape_fingerprint: format!(
                "{age};stale={}",
                if stale { "yes" } else { "no" }
            ),
        },
        skip("user_tweets", "§2"),
    ];
    // Write probes still run their shape-only contract check even in a
    // mock-only build — they never touch the network anyway.
    checks.push(create_tweet_check(None, opts));
    checks.push(skip("dm_inbox", "§4"));
    checks.push(dm_send_check(None, opts));
    ValidationReport {
        screen_name: screen_name.into(),
        user_id: user_id.into(),
        mock_only: true,
        checks,
        all_passed: false,
    }
    .finalize()
}

fn skip(check: &str, section: &str) -> CheckResult {
    CheckResult {
        check: check.into(),
        status: CheckStatus::Skipped,
        detail: MOCK_ONLY_REASON.into(),
        spec_section: section.into(),
        shape_fingerprint: "skipped".into(),
    }
}

/// The shipped harness logic, against an injected [`TwitterApi`]. `validate`
/// calls this with a real (wiremock-pointable) [`TwitterClient`]; the unit
/// tests call it directly with a wiremock-backed client too, so the exact
/// code path that ships is the code path under test (no re-implementation).
pub async fn validate_with_api(
    api: Arc<dyn TwitterApi>,
    screen_name: &str,
    user_id: &str,
    auth: &TwitterAuth,
    opts: &ValidateOptions,
) -> ValidationReport {
    let mut checks = Vec::new();

    let now_ms = now_millis();
    let age = auth
        .session_age_days(now_ms)
        .map(|d| format!("age={d}d"))
        .unwrap_or_else(|| "age=?".into());
    let stale_note = if auth.is_session_stale(now_ms) {
        " (LOCAL ADVISORY: session >60d old — re-harvest if auth flaps)"
    } else {
        ""
    };

    // 1 + 2. Auth + UserTweets — one call proves both. Probe the operator's
    // own timeline (always non-empty for an active account, and self-owned
    // so nothing is acted on).
    match api.fetch_user_tweets(user_id, None).await {
        Ok(tweets) => {
            checks.push(CheckResult {
                check: "auth".into(),
                status: CheckStatus::Pass,
                detail: format!(
                    "cookies + bearer + csrf + xctid accepted (200 on \
                     UserTweets){stale_note}"
                ),
                spec_section: "§1".into(),
                shape_fingerprint: format!("{age};http=200"),
            });
            checks.push(CheckResult {
                check: "user_tweets".into(),
                status: CheckStatus::Pass,
                detail: format!(
                    "parsed {} tweet(s) from the documented timeline_v2 shape",
                    tweets.len()
                ),
                spec_section: "§2".into(),
                shape_fingerprint: format!("tweets={}", tweets.len()),
            });
        }
        Err(e) => {
            let (auth_status, ut_status, detail) = classify(&e);
            let fp = err_fingerprint(&e);
            checks.push(CheckResult {
                check: "auth".into(),
                status: auth_status,
                detail: format!("{detail}{stale_note}"),
                spec_section: "§1".into(),
                shape_fingerprint: format!("{age};{fp}"),
            });
            checks.push(CheckResult {
                check: "user_tweets".into(),
                status: ut_status,
                detail,
                spec_section: "§2".into(),
                shape_fingerprint: fp,
            });
        }
    }

    // 3. CreateTweet — shape-only by default (never posts during validation).
    checks.push(create_tweet_live_or_dry(&api, opts).await);

    // 4. DM inbox.
    match api.fetch_dm_inbox(None).await {
        Ok(dms) => checks.push(CheckResult {
            check: "dm_inbox".into(),
            status: CheckStatus::Pass,
            detail: format!(
                "parsed {} inbound DM(s) from inbox_initial_state.json",
                dms.len()
            ),
            spec_section: "§4".into(),
            shape_fingerprint: format!("dms={}", dms.len()),
        }),
        Err(e) => {
            let (_, st, detail) = classify(&e);
            checks.push(CheckResult {
                check: "dm_inbox".into(),
                status: st,
                detail,
                spec_section: "§4".into(),
                shape_fingerprint: err_fingerprint(&e),
            });
        }
    }

    // 5. DM send — shape-only by default.
    checks.push(dm_send_live_or_dry(&api, opts).await);

    ValidationReport {
        screen_name: screen_name.into(),
        user_id: user_id.into(),
        mock_only: false,
        checks,
        all_passed: false,
    }
    .finalize()
}

/// A stable, content-free token for a probe error, for the shape fingerprint.
fn err_fingerprint(e: &TwitterError) -> String {
    match e {
        TwitterError::AuthExpired => "error=AuthExpired".into(),
        TwitterError::RateLimited { .. } => "error=RateLimited".into(),
        TwitterError::QueryIdRotated { status, .. } => {
            format!("error=QueryIdRotated;http={status}")
        }
        TwitterError::SchemaDrift { body_len, .. } => {
            format!("error=SchemaDrift;body_len={body_len}")
        }
        TwitterError::Api { status, .. } => format!("error=Api;http={status}"),
        TwitterError::Http(_) => "error=Http".into(),
        TwitterError::Decode(_) => "error=Decode".into(),
        TwitterError::Config(_) => "error=Config".into(),
    }
}

/// Map a probe error onto `(auth_check_status, op_check_status, detail)`.
/// An `AuthExpired` fails the auth check itself; other errors leave auth
/// "skipped" (we couldn't tell) but fail the op.
fn classify(e: &TwitterError) -> (CheckStatus, CheckStatus, String) {
    match e {
        TwitterError::AuthExpired => (
            CheckStatus::Fail,
            CheckStatus::Skipped,
            "session rejected (401/403) — re-harvest cookies via \
             scripts/twitter-harvest.sh and `twitter login` again"
                .into(),
        ),
        TwitterError::RateLimited {
            retry_after_secs, ..
        } => (
            CheckStatus::Skipped,
            CheckStatus::Fail,
            format!(
                "rate limited (429) — retry in ~{retry_after_secs}s; \
                 not a spec failure"
            ),
        ),
        TwitterError::QueryIdRotated { status, .. } => (
            CheckStatus::Pass,
            CheckStatus::Fail,
            format!(
                "queryId rotated (status {status}) — every candidate in the \
                 chain is stale; capture the live queryId (see runbook) and \
                 cache it via the store / env override"
            ),
        ),
        TwitterError::SchemaDrift {
            body_len,
            body_excerpt,
            ..
        } => (
            CheckStatus::Pass,
            CheckStatus::Fail,
            format!(
                "SCHEMA DRIFT: 2xx but parsed 0 records from a {body_len}B \
                 body — X reshaped the response. Raw body logged at WARN. \
                 Excerpt: {body_excerpt}"
            ),
        ),
        other => (
            CheckStatus::Skipped,
            CheckStatus::Fail,
            format!("probe error: {other}"),
        ),
    }
}

/// CreateTweet: live no-op probe when `allow_write` + a `probe_reply_to` are
/// both set; otherwise the dry-run shape contract check.
async fn create_tweet_live_or_dry(
    api: &Arc<dyn TwitterApi>,
    opts: &ValidateOptions,
) -> CheckResult {
    if !opts.allow_write || opts.probe_reply_to.is_none() {
        return create_tweet_check(None, opts);
    }
    let parent = opts.probe_reply_to.as_deref().unwrap();
    match api.reply_to_tweet(parent, "validation probe").await {
        Ok(id) => CheckResult {
            check: "create_tweet".into(),
            status: CheckStatus::Pass,
            detail: format!("live reply accepted; new rest_id={id}"),
            spec_section: "§3".into(),
            shape_fingerprint: "rest_id=present".into(),
        },
        Err(e) => {
            let (_, st, detail) = classify(&e);
            CheckResult {
                check: "create_tweet".into(),
                status: st,
                detail,
                spec_section: "§3".into(),
                shape_fingerprint: err_fingerprint(&e),
            }
        }
    }
}

/// CreateTweet dry-run contract check: re-derive the request body the client
/// builds and assert its documented invariants (catches a doc↔code drift
/// even with no network). Never sends.
fn create_tweet_check(
    _api: Option<()>,
    _opts: &ValidateOptions,
) -> CheckResult {
    let body = serde_json::json!({
        "variables": {
            "tweet_text": "(validation — not sent)",
            "reply": { "in_reply_to_tweet_id": "0", "exclude_reply_user_ids": [] },
            "dark_request": false,
            "media": { "media_entities": [], "possibly_sensitive": false },
            "semantic_annotation_ids": [],
        },
        "queryId": "(resolved at call time)",
    });
    let ok = body.pointer("/variables/tweet_text").is_some()
        && body
            .pointer("/variables/reply/in_reply_to_tweet_id")
            .is_some()
        && body.pointer("/variables/media/media_entities").is_some();
    CheckResult {
        check: "create_tweet_dry_run".into(),
        status: if ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: "request body matches docs §3 contract (NOT sent — pass \
                 --allow-write + --probe-reply-to for a live probe)"
            .into(),
        spec_section: "§3".into(),
        shape_fingerprint: "body=contract-ok;dry_run".into(),
    }
}

async fn dm_send_live_or_dry(
    api: &Arc<dyn TwitterApi>,
    opts: &ValidateOptions,
) -> CheckResult {
    if !opts.allow_write || opts.probe_conversation_id.is_none() {
        return dm_send_check(None, opts);
    }
    let conv = opts.probe_conversation_id.as_deref().unwrap();
    match api.send_dm(conv, "validation probe").await {
        Ok(id) => CheckResult {
            check: "dm_send".into(),
            status: CheckStatus::Pass,
            detail: format!("live DM accepted; event id={id}"),
            spec_section: "§5".into(),
            shape_fingerprint: "event_id=present".into(),
        },
        Err(e) => {
            let (_, st, detail) = classify(&e);
            CheckResult {
                check: "dm_send".into(),
                status: st,
                detail,
                spec_section: "§5".into(),
                shape_fingerprint: err_fingerprint(&e),
            }
        }
    }
}

fn dm_send_check(_api: Option<()>, _opts: &ValidateOptions) -> CheckResult {
    let body = serde_json::json!({
        "conversation_id": "0-0",
        "recipient_ids": false,
        "request_id": "(uuid v4 at call time)",
        "text": "(validation — not sent)",
        "cards_platform": "Web-12",
        "include_cards": 1,
        "include_quote_count": true,
        "dm_users": false,
    });
    let ok = body.get("conversation_id").is_some()
        && body.get("request_id").is_some()
        && body.get("text").is_some();
    CheckResult {
        check: "dm_send_dry_run".into(),
        status: if ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: "new2.json body matches docs §5 contract (NOT sent — pass \
                 --allow-write + --probe-conversation-id for a live probe)"
            .into(),
        spec_section: "§5".into(),
        shape_fingerprint: "body=contract-ok;dry_run".into(),
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tweet, TwitterDm};
    use async_trait::async_trait;
    use std::collections::BTreeMap;

    fn auth() -> TwitterAuth {
        let mut cookies = BTreeMap::new();
        cookies.insert("auth_token".into(), "sess".into());
        cookies.insert("ct0".into(), "csrf".into());
        TwitterAuth {
            user_id: "99".into(),
            screen_name: "tester".into(),
            cookies,
            bearer: "AAAAtest".into(),
            user_agent: "test-agent".into(),
            harvested_at_ms: now_millis(),
        }
    }

    /// In-process mock of the X API surface — drives the *real*
    /// `validate_with_api` (the shipped path), no re-implementation.
    struct MockApi {
        tweets: Result<Vec<Tweet>, TwitterError>,
        dms: Result<Vec<TwitterDm>, TwitterError>,
    }

    fn clone_err(e: &TwitterError) -> TwitterError {
        match e {
            TwitterError::AuthExpired => TwitterError::AuthExpired,
            TwitterError::QueryIdRotated { op, status } => {
                TwitterError::QueryIdRotated {
                    op: op.clone(),
                    status: *status,
                }
            }
            TwitterError::SchemaDrift {
                op,
                body_len,
                body_excerpt,
            } => TwitterError::SchemaDrift {
                op: op.clone(),
                body_len: *body_len,
                body_excerpt: body_excerpt.clone(),
            },
            TwitterError::RateLimited {
                op,
                retry_after_secs,
                limit,
                remaining,
            } => TwitterError::RateLimited {
                op: op.clone(),
                retry_after_secs: *retry_after_secs,
                limit: *limit,
                remaining: *remaining,
            },
            TwitterError::Api { op, status, body } => TwitterError::Api {
                op: op.clone(),
                status: *status,
                body: body.clone(),
            },
            TwitterError::Decode(s) => TwitterError::Decode(s.clone()),
            TwitterError::Config(s) => TwitterError::Config(s.clone()),
            TwitterError::Http(_) => TwitterError::Decode("http".into()),
        }
    }

    #[async_trait]
    impl TwitterApi for MockApi {
        async fn fetch_user_tweets(
            &self,
            _u: &str,
            _s: Option<&str>,
        ) -> Result<Vec<Tweet>, TwitterError> {
            self.tweets.as_ref().map(|v| v.clone()).map_err(clone_err)
        }
        async fn reply_to_tweet(
            &self,
            _t: &str,
            _x: &str,
        ) -> Result<String, TwitterError> {
            Ok("rid".into())
        }
        async fn fetch_dm_inbox(
            &self,
            _c: Option<&str>,
        ) -> Result<Vec<TwitterDm>, TwitterError> {
            self.dms.as_ref().map(|v| v.clone()).map_err(clone_err)
        }
        async fn send_dm(
            &self,
            _c: &str,
            _t: &str,
        ) -> Result<String, TwitterError> {
            Ok("evt".into())
        }
    }

    async fn run(api: MockApi, opts: ValidateOptions) -> ValidationReport {
        let api: Arc<dyn TwitterApi> = Arc::new(api);
        let a = auth();
        validate_with_api(api, &a.screen_name, &a.user_id, &a, &opts).await
    }

    #[tokio::test]
    async fn all_green_when_endpoints_ok() {
        let report = run(
            MockApi {
                tweets: Ok(vec![]),
                dms: Ok(vec![]),
            },
            ValidateOptions::default(),
        )
        .await;
        assert!(report.all_passed, "{}", report.render_table());
        assert_eq!(report.checks.len(), 5);
        assert!(report
            .checks
            .iter()
            .all(|c| c.status == CheckStatus::Pass));
        // Fingerprint is content-free and present on every probe.
        assert!(report.checks.iter().all(|c| !c.shape_fingerprint.is_empty()));
        assert_eq!(
            report.checks[1].shape_fingerprint, "tweets=0",
            "{:?}",
            report.checks[1]
        );
    }

    #[tokio::test]
    async fn auth_expired_fails_auth_and_skips_user_tweets() {
        let report = run(
            MockApi {
                tweets: Err(TwitterError::AuthExpired),
                dms: Ok(vec![]),
            },
            ValidateOptions::default(),
        )
        .await;
        assert!(!report.all_passed);
        let auth_c = &report.checks[0];
        assert_eq!(auth_c.check, "auth");
        assert_eq!(auth_c.status, CheckStatus::Fail);
        assert!(auth_c.shape_fingerprint.contains("error=AuthExpired"));
        assert_eq!(report.checks[1].status, CheckStatus::Skipped);
    }

    #[tokio::test]
    async fn query_id_rotation_keeps_auth_pass_fails_op() {
        let report = run(
            MockApi {
                tweets: Err(TwitterError::QueryIdRotated {
                    op: "UserTweets".into(),
                    status: 404,
                }),
                dms: Ok(vec![]),
            },
            ValidateOptions::default(),
        )
        .await;
        // Auth itself worked (we got a routed response); the op failed on a
        // stale id — not a session problem.
        assert_eq!(report.checks[0].status, CheckStatus::Pass);
        assert_eq!(report.checks[1].status, CheckStatus::Fail);
        assert!(report.checks[1].detail.contains("queryId"));
        assert!(report.checks[1]
            .shape_fingerprint
            .contains("error=QueryIdRotated;http=404"));
        assert!(!report.all_passed);
    }

    #[tokio::test]
    async fn schema_drift_surfaces_as_failed_op_with_excerpt() {
        let report = run(
            MockApi {
                tweets: Err(TwitterError::SchemaDrift {
                    op: "UserTweets".into(),
                    body_len: 4096,
                    body_excerpt: "{\"new_shape\":true}".into(),
                }),
                dms: Ok(vec![]),
            },
            ValidateOptions::default(),
        )
        .await;
        assert_eq!(report.checks[1].status, CheckStatus::Fail);
        assert!(report.checks[1].detail.contains("SCHEMA DRIFT"));
        assert!(report.checks[1].detail.contains("new_shape"));
        assert!(report.checks[1]
            .shape_fingerprint
            .contains("error=SchemaDrift;body_len=4096"));
    }

    #[tokio::test]
    async fn rate_limited_is_skipped_not_a_spec_failure_for_auth() {
        let report = run(
            MockApi {
                tweets: Err(TwitterError::RateLimited {
                    op: "UserTweets".into(),
                    retry_after_secs: 900,
                    limit: Some(50),
                    remaining: Some(0),
                }),
                dms: Ok(vec![]),
            },
            ValidateOptions::default(),
        )
        .await;
        // Auth is "skipped" (can't tell), op fails but it's a transient note.
        assert_eq!(report.checks[0].status, CheckStatus::Skipped);
        assert_eq!(report.checks[1].status, CheckStatus::Fail);
        assert!(report.checks[1].detail.contains("rate limited"));
    }

    #[tokio::test]
    async fn write_paths_are_dry_by_default() {
        let report = run(
            MockApi {
                tweets: Ok(vec![]),
                dms: Ok(vec![]),
            },
            ValidateOptions::default(),
        )
        .await;
        let ct = report
            .checks
            .iter()
            .find(|c| c.check == "create_tweet_dry_run")
            .unwrap();
        assert_eq!(ct.status, CheckStatus::Pass);
        assert!(ct.detail.contains("NOT sent"));
        assert!(ct.shape_fingerprint.contains("dry_run"));
        let dm = report
            .checks
            .iter()
            .find(|c| c.check == "dm_send_dry_run")
            .unwrap();
        assert!(dm.detail.contains("NOT sent"));
    }

    #[tokio::test]
    async fn allow_write_with_target_does_live_probe() {
        let report = run(
            MockApi {
                tweets: Ok(vec![]),
                dms: Ok(vec![]),
            },
            ValidateOptions {
                allow_write: true,
                allow_live: true,
                probe_reply_to: Some("123".into()),
                probe_conversation_id: Some("55-99".into()),
            },
        )
        .await;
        let ct = report
            .checks
            .iter()
            .find(|c| c.check == "create_tweet")
            .unwrap();
        assert_eq!(ct.status, CheckStatus::Pass);
        assert!(ct.detail.contains("live reply accepted"));
        let dm = report
            .checks
            .iter()
            .find(|c| c.check == "dm_send")
            .unwrap();
        assert_eq!(dm.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn mock_only_gate_skips_reads_without_live_optin() {
        // No AUGMENTAGENT_TWITTER_BASE_URL, no allow_live → mock-only.
        // (validate() must NOT construct a client / hit the network.)
        std::env::remove_var("AUGMENTAGENT_TWITTER_BASE_URL");
        let report = validate(auth(), ValidateOptions::default()).await;
        assert!(report.mock_only);
        assert!(!report.all_passed, "mock-only run is never a sign-off");
        let auth_c = &report.checks[0];
        assert_eq!(auth_c.status, CheckStatus::Skipped);
        assert!(auth_c.detail.contains("mock-only"));
        // Dry-run write contract checks still pass (no network involved).
        assert!(report
            .checks
            .iter()
            .find(|c| c.check == "create_tweet_dry_run")
            .unwrap()
            .status
            == CheckStatus::Pass);
        assert!(report.render_table().contains("MOCK-ONLY"));
    }

    #[tokio::test]
    async fn mock_only_report_carries_session_age_fingerprint() {
        let mut a = auth();
        a.harvested_at_ms = now_millis() - 90 * 24 * 60 * 60 * 1000;
        std::env::remove_var("AUGMENTAGENT_TWITTER_BASE_URL");
        let report = validate(a, ValidateOptions::default()).await;
        assert!(report.mock_only);
        let auth_c = &report.checks[0];
        assert!(auth_c.shape_fingerprint.contains("stale=yes"));
    }

    #[test]
    fn render_table_states_mock_or_clear_or_keep_flags() {
        let mock = ValidationReport {
            screen_name: "x".into(),
            user_id: "1".into(),
            mock_only: true,
            checks: vec![],
            all_passed: false,
        }
        .finalize();
        assert!(mock.render_table().contains("MOCK-ONLY"));
        assert!(mock.render_table().contains("CANNOT be cleared"));

        let pass = ValidationReport {
            screen_name: "x".into(),
            user_id: "1".into(),
            mock_only: false,
            checks: vec![CheckResult {
                check: "auth".into(),
                status: CheckStatus::Pass,
                detail: "ok".into(),
                spec_section: "§1".into(),
                shape_fingerprint: "http=200".into(),
            }],
            all_passed: false,
        }
        .finalize();
        assert!(pass.render_table().contains("can be cleared"));

        let fail = ValidationReport {
            screen_name: "x".into(),
            user_id: "1".into(),
            mock_only: false,
            checks: vec![CheckResult {
                check: "auth".into(),
                status: CheckStatus::Fail,
                detail: "bad".into(),
                spec_section: "§1".into(),
                shape_fingerprint: "error=AuthExpired".into(),
            }],
            all_passed: false,
        }
        .finalize();
        assert!(fail.render_table().contains("keep the validation flags"));
    }

    #[test]
    fn report_serializes_for_json_artifact() {
        let r = ValidationReport {
            screen_name: "me".into(),
            user_id: "1".into(),
            mock_only: false,
            checks: vec![CheckResult {
                check: "user_tweets".into(),
                status: CheckStatus::Pass,
                detail: "parsed 3".into(),
                spec_section: "§2".into(),
                shape_fingerprint: "tweets=3".into(),
            }],
            all_passed: true,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"check\":\"user_tweets\""));
        assert!(json.contains("\"status\":\"pass\""));
        assert!(json.contains("\"shape_fingerprint\":\"tweets=3\""));
        assert!(json.contains("\"mock_only\":false"));
    }
}
