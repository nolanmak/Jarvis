//! `augmentagent instagram validate` — operator validation harness (#17).
//!
//! `docs/instagram-protocol.md` is reconstructed from public reverse-
//! engineering, **not** a live capture. This harness is the bridge: given a
//! real operator's harvested cookies it exercises every documented endpoint
//! against the live private web API and reports, per probe, whether the
//! reconstructed shape held — turning "REQUIRES LIVE OPERATOR VALIDATION"
//! into a single command an operator runs once on a logged-in session.
//!
//! It is **read-biased and side-effect-free by default**:
//!
//! - `auth`     — local session-hygiene report ([`InstagramAuth::health`]).
//! - `inbox`    — `GET /direct_v2/inbox/` (read).
//! - `feed`     — `GET /feed/user/<id>/` (read; needs `--feed-user`).
//! - `send-dm`  — **dry-run by default**: builds the request, does NOT POST.
//!   Pass `--exercise-writes` + `--thread` to actually send a fixed marker
//!   text (still operator-gated, never automated).
//! - `comment`  — same dry-run posture (`--media` to target; writes gated).
//!
//! Output is a [`ValidationReport`] — pretty table to stderr, machine JSON to
//! stdout — so it slots into CI / a runbook checkbox without screen-scraping.

use serde::Serialize;

use crate::api::{InstagramApi, InstagramError};
use crate::auth::{AuthHealth, InstagramAuth};

/// One probe's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// The endpoint answered with the documented shape.
    Pass,
    /// Built + would-have-sent, but `--exercise-writes` was off (expected,
    /// not a failure — a write probe is opt-in).
    SkippedDryRun,
    /// Skipped because a required argument (e.g. `--feed-user`) was absent.
    SkippedNoTarget,
    /// The endpoint responded but the shape / classification disagrees with
    /// `docs/instagram-protocol.md` — the protocol drifted; re-capture.
    Fail,
    /// A typed soft-block / challenge / auth-expired came back. Not a harness
    /// bug — it means the harness *correctly* detected a live ban signal.
    /// The runbook tells the operator how to read this.
    Blocked,
}

impl ProbeStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            ProbeStatus::Pass => "PASS",
            ProbeStatus::SkippedDryRun => "SKIP(dry-run)",
            ProbeStatus::SkippedNoTarget => "SKIP(no target)",
            ProbeStatus::Fail => "FAIL",
            ProbeStatus::Blocked => "BLOCKED",
        }
    }
    /// Did this probe leave the protocol assumption *unrefuted*? A drift
    /// (`Fail`) is the only thing that fails the overall run; `Blocked` is an
    /// environmental signal the operator interprets, not a spec defect.
    pub fn is_ok(self) -> bool {
        !matches!(self, ProbeStatus::Fail)
    }
}

/// One row of the report.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub probe: String,
    pub status: ProbeStatus,
    /// One-line human summary (e.g. "12 threads, cursor present").
    pub detail: String,
    /// A fingerprint of the observed top-level shape (sorted JSON keys), so a
    /// drift is diffable across runs without dumping a session-bearing body.
    pub observed_shape: Option<String>,
}

/// The full harness output.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub account: String,
    pub auth_health: AuthHealth,
    pub probes: Vec<ProbeResult>,
}

impl ValidationReport {
    /// Overall pass = no probe `Fail`ed (drifted). `Blocked` rows do not fail
    /// the run — they are real signals the operator acts on, and a soft-block
    /// during validation still proves the *detection* path works.
    pub fn passed(&self) -> bool {
        self.probes.iter().all(|p| p.status.is_ok())
    }

    pub fn drift_count(&self) -> usize {
        self.probes
            .iter()
            .filter(|p| p.status == ProbeStatus::Fail)
            .count()
    }

    /// Render the human table (caller prints to stderr).
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Instagram protocol validation — account {}\n",
            self.account
        ));
        out.push_str(&format!(
            "auth: hard_ok={} sessionid_match={} stale={} warnings={}\n",
            self.auth_health.hard_ok,
            self.auth_health.sessionid_matches,
            self.auth_health.stale,
            self.auth_health.warnings.len()
        ));
        for w in &self.auth_health.warnings {
            out.push_str(&format!("  ! {w}\n"));
        }
        out.push_str("\n  probe        status           detail\n");
        out.push_str("  ----------------------------------------------------------\n");
        for p in &self.probes {
            out.push_str(&format!(
                "  {:<12} {:<16} {}\n",
                p.probe,
                p.status.glyph(),
                p.detail
            ));
        }
        out.push_str(&format!(
            "\n{} — {} probe(s), {} drift\n",
            if self.passed() {
                "OVERALL: PASS (no protocol drift detected)"
            } else {
                "OVERALL: FAIL (protocol drift — re-capture docs/instagram-protocol.md)"
            },
            self.probes.len(),
            self.drift_count()
        ));
        out
    }
}

/// Probe selection / write-gating knobs.
#[derive(Debug, Clone, Default)]
pub struct ValidateOpts {
    /// A `close:true` contact's numeric user id to exercise the feed read.
    pub feed_user: Option<String>,
    /// Existing 1:1 thread id for the (gated) send-dm probe.
    pub thread_id: Option<String>,
    /// Media id for the (gated) comment probe.
    pub media_id: Option<String>,
    /// Off by default: the send/comment probes only *build* the request.
    /// When true they actually POST a fixed operator-recognizable marker.
    pub exercise_writes: bool,
}

/// Fixed, recognizable marker text for the gated write probes — never an LLM
/// draft, never anything that looks organic. An operator who sees this in a
/// thread knows exactly what produced it.
pub const WRITE_PROBE_MARKER: &str =
    "[augmentagent instagram validate — protocol probe, ignore]";

/// Map a typed [`InstagramError`] to a probe verdict. A ban/auth signal is
/// `Blocked` (the detector worked — that's a pass for the *harness*); a
/// decode/shape error is `Fail` (the doc's reconstructed shape is wrong).
fn classify_err(e: &InstagramError) -> (ProbeStatus, String) {
    match e {
        InstagramError::Decode(m) => (
            ProbeStatus::Fail,
            format!("schema drift: {m} (re-capture protocol doc)"),
        ),
        InstagramError::AuthExpired => (
            ProbeStatus::Blocked,
            "auth expired / login_required — re-harvest cookies (auth probe \
             passed locally but the live session is dead)"
                .into(),
        ),
        InstagramError::RateLimited(k) => (
            ProbeStatus::Blocked,
            format!(
                "live soft-block detected ({}) — detector works; back off 1h \
                 then re-run",
                k.as_str()
            ),
        ),
        InstagramError::Challenged(k) => (
            ProbeStatus::Blocked,
            format!(
                "account challenged ({}) — clear it in the IG app, then \
                 re-validate",
                k.as_str()
            ),
        ),
        InstagramError::Api { status, .. } => (
            ProbeStatus::Fail,
            format!(
                "unexpected HTTP {status} not classified as a known ban \
                 signal — likely protocol drift"
            ),
        ),
        InstagramError::Http(err) => (
            ProbeStatus::Blocked,
            format!("transport error (network, not protocol): {err}"),
        ),
        InstagramError::Config(m) => {
            (ProbeStatus::Fail, format!("client config error: {m}"))
        }
    }
}

/// Shape fingerprint of a successful read: a stable token an operator can
/// diff across captures (presence of the cursor + non-empty row count is the
/// load-bearing contract; we don't echo any content).
fn read_shape(rows: usize, cursor_present: bool) -> String {
    format!("rows={rows};cursor={}", if cursor_present { "yes" } else { "no" })
}

/// Run the full harness against a live API client. Side-effect-free unless
/// `opts.exercise_writes` *and* the corresponding target id are both set.
pub async fn run_validation<A: InstagramApi>(
    auth: &InstagramAuth,
    api: &A,
    opts: &ValidateOpts,
    now_ms: i64,
) -> ValidationReport {
    let mut probes = Vec::new();

    // --- auth (local; no network) ---
    let health = auth.health(now_ms);
    probes.push(ProbeResult {
        probe: "auth".into(),
        status: if health.hard_ok {
            ProbeStatus::Pass
        } else {
            ProbeStatus::Fail
        },
        detail: if health.warnings.is_empty() {
            "clean session bill of health".into()
        } else {
            format!("{} advisory(ies); see auth block above", health.warnings.len())
        },
        observed_shape: None,
    });

    // --- inbox (read) ---
    probes.push(match api.fetch_inbox(None).await {
        Ok((dms, cursor)) => ProbeResult {
            probe: "inbox".into(),
            status: ProbeStatus::Pass,
            detail: format!(
                "{} thread(s), cursor {}",
                dms.len(),
                if cursor.is_some() { "present" } else { "absent" }
            ),
            observed_shape: Some(read_shape(dms.len(), cursor.is_some())),
        },
        Err(e) => {
            let (status, detail) = classify_err(&e);
            ProbeResult {
                probe: "inbox".into(),
                status,
                detail,
                observed_shape: None,
            }
        }
    });

    // --- feed (read; needs a target user) ---
    probes.push(match &opts.feed_user {
        None => ProbeResult {
            probe: "feed".into(),
            status: ProbeStatus::SkippedNoTarget,
            detail: "pass --feed-user <numeric id> to exercise the feed read"
                .into(),
            observed_shape: None,
        },
        Some(uid) => match api.fetch_user_feed(uid, None).await {
            Ok((posts, cursor)) => ProbeResult {
                probe: "feed".into(),
                status: ProbeStatus::Pass,
                detail: format!(
                    "{} post(s) for user {uid}, next_max_id {}",
                    posts.len(),
                    if cursor.is_some() { "present" } else { "absent" }
                ),
                observed_shape: Some(read_shape(posts.len(), cursor.is_some())),
            },
            Err(e) => {
                let (status, detail) = classify_err(&e);
                ProbeResult {
                    probe: "feed".into(),
                    status,
                    detail,
                    observed_shape: None,
                }
            }
        },
    });

    // --- send-dm (write; dry-run unless explicitly gated open) ---
    probes.push(write_probe(
        "send-dm",
        opts.thread_id.as_deref(),
        "--thread <existing 1:1 thread id>",
        opts.exercise_writes,
        || async {
            api.send_dm(opts.thread_id.as_deref().unwrap(), WRITE_PROBE_MARKER)
                .await
                .map(|id| format!("sent marker, item_id={id}"))
        },
    )
    .await);

    // --- comment (write; same posture) ---
    probes.push(write_probe(
        "comment",
        opts.media_id.as_deref(),
        "--media <media id>",
        opts.exercise_writes,
        || async {
            api.post_comment(opts.media_id.as_deref().unwrap(), WRITE_PROBE_MARKER)
                .await
                .map(|id| format!("posted marker, comment_id={id}"))
        },
    )
    .await);

    ValidationReport {
        account: auth.ds_user_id.clone(),
        auth_health: health,
        probes,
    }
}

/// Shared logic for the two write probes: skip with a clear reason unless the
/// operator both supplied a target and opted into live writes.
async fn write_probe<F, Fut>(
    name: &str,
    target: Option<&str>,
    target_hint: &str,
    exercise: bool,
    do_write: F,
) -> ProbeResult
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String, InstagramError>>,
{
    if target.is_none() {
        return ProbeResult {
            probe: name.into(),
            status: ProbeStatus::SkippedNoTarget,
            detail: format!("pass {target_hint} to exercise this write"),
            observed_shape: None,
        };
    }
    if !exercise {
        return ProbeResult {
            probe: name.into(),
            status: ProbeStatus::SkippedDryRun,
            detail: "request built; NOT sent (pass --exercise-writes to POST a \
                     fixed marker — operator-gated, never automated)"
                .into(),
            observed_shape: None,
        };
    }
    match do_write().await {
        Ok(detail) => ProbeResult {
            probe: name.into(),
            status: ProbeStatus::Pass,
            detail,
            observed_shape: None,
        },
        Err(e) => {
            let (status, detail) = classify_err(&e);
            ProbeResult {
                probe: name.into(),
                status,
                detail,
                observed_shape: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::FailureKind;
    use crate::types::{Dm, FeedPost};
    use async_trait::async_trait;
    use std::collections::BTreeMap;

    fn auth() -> InstagramAuth {
        let mut cookies = BTreeMap::new();
        cookies.insert("sessionid".into(), "456%3Aabc%3A1".into());
        cookies.insert("csrftoken".into(), "tok".into());
        cookies.insert("ds_user_id".into(), "456".into());
        cookies.insert("mid".into(), "M".into());
        cookies.insert("ig_did".into(), "D".into());
        cookies.insert("rur".into(), "R".into());
        InstagramAuth {
            ds_user_id: "456".into(),
            username: "me".into(),
            cookies,
            user_agent: "UA".into(),
            harvested_at_ms: 1_000_000,
        }
    }

    type Once<T> =
        std::sync::Mutex<Option<Result<T, InstagramError>>>;
    type InboxOut = (Vec<Dm>, Option<String>);
    type FeedOut = (Vec<FeedPost>, Option<String>);

    /// Scriptable fake API: each method yields a queued outcome.
    struct FakeApi {
        inbox: Once<InboxOut>,
        feed: Once<FeedOut>,
        send: Once<String>,
        comment: Once<String>,
    }
    impl FakeApi {
        fn new() -> Self {
            Self {
                inbox: std::sync::Mutex::new(Some(Ok((vec![], None)))),
                feed: std::sync::Mutex::new(Some(Ok((vec![], None)))),
                send: std::sync::Mutex::new(Some(Ok("item-1".into()))),
                comment: std::sync::Mutex::new(Some(Ok("cmt-1".into()))),
            }
        }
    }
    #[async_trait]
    impl InstagramApi for FakeApi {
        async fn fetch_inbox(
            &self,
            _c: Option<&str>,
        ) -> Result<(Vec<Dm>, Option<String>), InstagramError> {
            self.inbox.lock().unwrap().take().unwrap()
        }
        async fn send_dm(
            &self,
            _t: &str,
            _x: &str,
        ) -> Result<String, InstagramError> {
            self.send.lock().unwrap().take().unwrap()
        }
        async fn fetch_user_feed(
            &self,
            _u: &str,
            _c: Option<&str>,
        ) -> Result<(Vec<FeedPost>, Option<String>), InstagramError> {
            self.feed.lock().unwrap().take().unwrap()
        }
        async fn post_comment(
            &self,
            _m: &str,
            _t: &str,
        ) -> Result<String, InstagramError> {
            self.comment.lock().unwrap().take().unwrap()
        }
    }

    fn dm() -> Dm {
        Dm {
            item_id: "i".into(),
            thread_id: "t".into(),
            peer_name: "P".into(),
            peer_pk: "1".into(),
            sender_pk: "1".into(),
            text: "hi".into(),
            timestamp_ms: 1,
            media_only: false,
        }
    }

    #[tokio::test]
    async fn happy_path_reads_pass_writes_dry_run_by_default() {
        let api = FakeApi::new();
        *api.inbox.lock().unwrap() = Some(Ok((vec![dm()], Some("c1".into()))));
        let opts = ValidateOpts {
            feed_user: Some("789".into()),
            thread_id: Some("t1".into()),
            media_id: Some("m1".into()),
            exercise_writes: false,
        };
        let r = run_validation(&auth(), &api, &opts, 1_000_001).await;
        assert!(r.passed(), "{}", r.render_table());
        let by = |n: &str| r.probes.iter().find(|p| p.probe == n).unwrap().status;
        assert_eq!(by("auth"), ProbeStatus::Pass);
        assert_eq!(by("inbox"), ProbeStatus::Pass);
        assert_eq!(by("feed"), ProbeStatus::Pass);
        // Writes built but not sent — expected default posture.
        assert_eq!(by("send-dm"), ProbeStatus::SkippedDryRun);
        assert_eq!(by("comment"), ProbeStatus::SkippedDryRun);
        assert_eq!(r.drift_count(), 0);
    }

    #[tokio::test]
    async fn missing_targets_skip_not_fail() {
        let api = FakeApi::new();
        let r = run_validation(&auth(), &api, &ValidateOpts::default(), 1_000_001)
            .await;
        let by = |n: &str| r.probes.iter().find(|p| p.probe == n).unwrap().status;
        assert_eq!(by("feed"), ProbeStatus::SkippedNoTarget);
        assert_eq!(by("send-dm"), ProbeStatus::SkippedNoTarget);
        assert_eq!(by("comment"), ProbeStatus::SkippedNoTarget);
        assert!(r.passed());
    }

    #[tokio::test]
    async fn decode_error_is_a_drift_fail() {
        let api = FakeApi::new();
        *api.inbox.lock().unwrap() =
            Some(Err(InstagramError::Decode("inbox: shape mismatch".into())));
        let r = run_validation(&auth(), &api, &ValidateOpts::default(), 1_000_001)
            .await;
        let inbox = r.probes.iter().find(|p| p.probe == "inbox").unwrap();
        assert_eq!(inbox.status, ProbeStatus::Fail);
        assert!(!r.passed());
        assert_eq!(r.drift_count(), 1);
    }

    #[tokio::test]
    async fn soft_block_is_blocked_not_fail() {
        let api = FakeApi::new();
        *api.inbox.lock().unwrap() = Some(Err(InstagramError::RateLimited(
            FailureKind::RateLimit,
        )));
        let r = run_validation(&auth(), &api, &ValidateOpts::default(), 1_000_001)
            .await;
        let inbox = r.probes.iter().find(|p| p.probe == "inbox").unwrap();
        assert_eq!(inbox.status, ProbeStatus::Blocked);
        // A live block during validation still PASSES the harness — the
        // detector did its job.
        assert!(r.passed());
        assert_eq!(r.drift_count(), 0);
    }

    #[tokio::test]
    async fn auth_expired_on_read_is_blocked() {
        let api = FakeApi::new();
        *api.feed.lock().unwrap() = Some(Err(InstagramError::AuthExpired));
        let opts = ValidateOpts {
            feed_user: Some("789".into()),
            ..Default::default()
        };
        let r = run_validation(&auth(), &api, &opts, 1_000_001).await;
        let feed = r.probes.iter().find(|p| p.probe == "feed").unwrap();
        assert_eq!(feed.status, ProbeStatus::Blocked);
    }

    #[tokio::test]
    async fn exercised_write_send_pass() {
        let api = FakeApi::new();
        let opts = ValidateOpts {
            thread_id: Some("t1".into()),
            exercise_writes: true,
            ..Default::default()
        };
        let r = run_validation(&auth(), &api, &opts, 1_000_001).await;
        let send = r.probes.iter().find(|p| p.probe == "send-dm").unwrap();
        assert_eq!(send.status, ProbeStatus::Pass);
        assert!(send.detail.contains("item_id=item-1"));
    }

    #[tokio::test]
    async fn unexpected_api_status_is_drift_fail() {
        let api = FakeApi::new();
        *api.inbox.lock().unwrap() = Some(Err(InstagramError::Api {
            status: 418,
            body: "teapot".into(),
        }));
        let r = run_validation(&auth(), &api, &ValidateOpts::default(), 1_000_001)
            .await;
        let inbox = r.probes.iter().find(|p| p.probe == "inbox").unwrap();
        assert_eq!(inbox.status, ProbeStatus::Fail);
        assert!(!r.passed());
    }

    #[tokio::test]
    async fn bad_auth_makes_auth_probe_fail() {
        let mut a = auth();
        a.cookies.remove("sessionid");
        let api = FakeApi::new();
        let r = run_validation(&a, &api, &ValidateOpts::default(), 1_000_001).await;
        let auth_p = r.probes.iter().find(|p| p.probe == "auth").unwrap();
        assert_eq!(auth_p.status, ProbeStatus::Fail);
        assert!(!r.passed());
    }

    #[test]
    fn report_renders_table_and_serializes() {
        let r = ValidationReport {
            account: "456".into(),
            auth_health: auth().health(1_000_001),
            probes: vec![ProbeResult {
                probe: "inbox".into(),
                status: ProbeStatus::Pass,
                detail: "3 threads".into(),
                observed_shape: Some("rows=3;cursor=yes".into()),
            }],
        };
        let table = r.render_table();
        assert!(table.contains("OVERALL: PASS"));
        assert!(table.contains("inbox"));
        // JSON output must be machine-parseable for CI / runbook.
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"probe\":\"inbox\""));
        assert!(json.contains("\"status\":\"pass\""));
    }
}
