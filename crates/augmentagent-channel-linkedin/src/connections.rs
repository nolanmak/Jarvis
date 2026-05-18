//! LinkedIn 1st-degree connection sync → dormant-contact bootstrap (#61).
//!
//! Reuses the existing Voyager auth ([`crate::auth::LinkedInAuth`]) — same
//! cookie jar + `csrf-token` header as the DM client. Walks
//! `/voyager/api/relationships/dash/connections` paginated `start=0,40,80,…`
//! until an empty page, then upserts each connection into the wiki via the
//! shared **fill-blanks-only** merger ([`augmentagent_wiki::merge_person_page`]).
//!
//! Rate discipline (issue §1): connection reads are *read* traffic but
//! LinkedIn rate-limits them aggressively. We enforce a **hard local
//! ceiling** independent of the RateGovernor (which governs *write*
//! traffic): ≤ 1 page / 5 s and ≤ 200 pages / 24 h. 429 → exponential
//! backoff with jitter starting at 30 s. Even a 5k-connection account
//! finishes a full sync in well under an hour.
//!
//! Two modes (issue §2):
//! - **Full sync** — first run, or `now - last_full_sync ≥ 30 d`. Walks
//!   every page; resumable via the persisted `cursor_start`.
//! - **Delta sync** — daily; stop the pager the moment a connection's
//!   `createdAt` predates `last_full_sync` (Voyager returns
//!   recency-descending).
//!
//! The default mode is a **dry run**: produce a JSON diff
//! ([`SyncReport`]) and write nothing. Wiki writes only happen when the
//! caller passes `apply = true`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::api::LinkedInError;
use crate::auth::LinkedInAuth;
use augmentagent_wiki::{merge_person_page, slug_from_email, PersonPatch, WikiLayout};

/// Connections returned per Voyager page. LinkedIn's relationships API is
/// fixed at 40; exposed as a const so tests and the pager agree.
pub const PAGE_SIZE: usize = 40;

/// Hard local ceiling: minimum gap between page fetches.
pub const MIN_PAGE_GAP: Duration = Duration::from_secs(5);

/// Hard local ceiling: maximum pages in any rolling sync invocation. 200
/// pages × 40 = 8 000 connections — comfortably above any real account.
pub const MAX_PAGES_PER_RUN: usize = 200;

/// 429 backoff base (issue §1: "start 30s"), doubled per consecutive 429.
pub const BACKOFF_BASE: Duration = Duration::from_secs(30);

/// Full sync re-runs at most every 30 days; otherwise a daily run is a delta.
pub const FULL_SYNC_INTERVAL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// One 1st-degree connection lifted from the Voyager relationships payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub first_name: String,
    pub last_name: String,
    /// LinkedIn vanity slug (`jane-doe-1234`) — the stable public identity.
    pub public_identifier: String,
    /// "Staff Engineer at Acme" style headline.
    pub headline: String,
    /// Best-effort current company (from the embedded mini-profile), may be
    /// empty — empty stays empty (never invented).
    pub company: String,
    /// Connection date (ms since epoch). 0 if Voyager omitted it.
    pub connected_at_ms: i64,
}

impl Connection {
    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name.trim(), self.last_name.trim())
            .trim()
            .to_string()
    }

    pub fn profile_url(&self) -> String {
        format!("https://www.linkedin.com/in/{}", self.public_identifier)
    }
}

/// Seam tests inject a fake into. One method: fetch a single page by
/// `start` offset. The pager owns iteration + the rate ceiling so the trait
/// stays trivial to fake.
#[async_trait]
pub trait ConnectionsApi: Send + Sync {
    /// Fetch the connections page at `start`. Empty `Vec` signals the end.
    async fn fetch_page(&self, start: usize) -> Result<Vec<Connection>, LinkedInError>;
}

/// Voyager-backed [`ConnectionsApi`]. Mirrors `api::VoyagerClient`'s header
/// construction (cookie jar + csrf-token + restli protocol headers).
pub struct VoyagerConnectionsClient {
    http: reqwest::Client,
    auth: LinkedInAuth,
}

impl VoyagerConnectionsClient {
    pub fn new(auth: LinkedInAuth) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self { http, auth }
    }

    fn headers(&self) -> Result<reqwest::header::HeaderMap, LinkedInError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut h = HeaderMap::new();
        let mut set = |name: &'static str, val: String| -> Result<(), LinkedInError> {
            let name = HeaderName::from_static(name);
            let value = HeaderValue::from_str(&val)
                .map_err(|e| LinkedInError::Config(format!("{name}: {e}")))?;
            h.insert(name, value);
            Ok(())
        };
        set("cookie", self.auth.cookie_header())?;
        set(
            "csrf-token",
            self.auth
                .csrf_token()
                .map_err(|e| LinkedInError::Config(e.to_string()))?,
        )?;
        set("x-restli-protocol-version", "2.0.0".into())?;
        set(
            "x-li-accept",
            "application/vnd.linkedin.normalized+json+2.1".into(),
        )?;
        set("accept", "*/*".into())?;
        set("referer", "https://www.linkedin.com/mynetwork/".into())?;
        set("origin", "https://www.linkedin.com".into())?;
        Ok(h)
    }
}

#[async_trait]
impl ConnectionsApi for VoyagerConnectionsClient {
    async fn fetch_page(&self, start: usize) -> Result<Vec<Connection>, LinkedInError> {
        let url = format!(
            "https://www.linkedin.com/voyager/api/relationships/dash/connections\
             ?decorationId=com.linkedin.voyager.dash.deco.web.mynetwork.ConnectionListWithProfile-16\
             &count={PAGE_SIZE}&q=search&sortType=RECENTLY_ADDED&start={start}"
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.headers()?)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if status.as_u16() == 429 {
            return Err(LinkedInError::Voyager {
                status: 429,
                body: "rate limited".into(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("connections json: {e}")))?;
        Ok(parse_connections(&v))
    }
}

/// Parse the Voyager `relationships/dash/connections` payload into our
/// flattened [`Connection`] list. Voyager nests the mini-profile under
/// `connectedMemberResolutionResult`; unknown shapes yield an empty list
/// (treated as end-of-pagination, never a panic).
pub fn parse_connections(v: &serde_json::Value) -> Vec<Connection> {
    let elements = v
        .get("elements")
        .or_else(|| v.get("data").and_then(|d| d.get("elements")))
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for el in &elements {
        let connected_at_ms = el
            .get("createdAt")
            .and_then(|c| c.as_i64())
            .unwrap_or(0);
        let prof = el
            .get("connectedMemberResolutionResult")
            .or_else(|| el.get("miniProfile"))
            .unwrap_or(el);
        let first_name = str_field(prof, "firstName");
        let last_name = str_field(prof, "lastName");
        let public_identifier = str_field(prof, "publicIdentifier");
        if public_identifier.is_empty() {
            continue;
        }
        let headline = str_field(prof, "headline")
            .or_else_str(|| str_field(prof, "occupation"));
        let company = company_from_headline(&headline);
        out.push(Connection {
            first_name,
            last_name,
            public_identifier,
            headline,
            company,
            connected_at_ms,
        });
    }
    out
}

/// `headline` is "Title at Company"; extract a best-effort company. Empty if
/// the pattern doesn't hold — we never invent.
fn company_from_headline(headline: &str) -> String {
    if let Some(idx) = headline.to_lowercase().find(" at ") {
        return headline[idx + 4..].trim().to_string();
    }
    String::new()
}

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Tiny ergonomic helper so the parser reads cleanly.
trait OrElseStr {
    fn or_else_str(self, f: impl FnOnce() -> String) -> String;
}
impl OrElseStr for String {
    fn or_else_str(self, f: impl FnOnce() -> String) -> String {
        if self.is_empty() {
            f()
        } else {
            self
        }
    }
}

/// Sync mode decided from the persisted cursor + clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// First run ever, or ≥ 30 d since the last full sync.
    Full,
    /// Daily incremental; stop at `createdAt < last_full_sync_ms`.
    Delta { last_full_sync_ms: i64 },
}

impl SyncMode {
    /// `last_full_sync_ms == None` (never synced) → Full. Otherwise Delta
    /// unless the full-sync interval has elapsed.
    pub fn decide(last_full_sync_ms: Option<i64>, now_ms: i64) -> Self {
        match last_full_sync_ms {
            None => SyncMode::Full,
            Some(prev) if now_ms - prev >= FULL_SYNC_INTERVAL_MS => SyncMode::Full,
            Some(prev) => SyncMode::Delta {
                last_full_sync_ms: prev,
            },
        }
    }
}

/// Per-connection plan: would this create a new stub, fill blanks on an
/// existing page, or leave it untouched?
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionDiff {
    pub public_identifier: String,
    pub name: String,
    pub slug: String,
    /// `create` | `update` | `noop`.
    pub action: String,
    /// Fields the merge filled (empty for noop).
    pub filled: Vec<String>,
}

/// Aggregate dry-run / applied report. Serializes to the JSON dump and backs
/// the Discord summary card ("N new / M updated").
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub mode: String,
    pub pages_fetched: usize,
    pub connections_seen: usize,
    pub created: usize,
    pub updated: usize,
    pub noop: usize,
    pub applied: bool,
    pub diffs: Vec<ConnectionDiff>,
}

impl SyncReport {
    /// One-line Discord summary card body.
    pub fn discord_summary(&self) -> String {
        format!(
            "**LinkedIn connections sync** ({})\n\
             {} new · {} updated · {} unchanged\n\
             {} pages · {} connections{}",
            self.mode,
            self.created,
            self.updated,
            self.noop,
            self.pages_fetched,
            self.connections_seen,
            if self.applied {
                ""
            } else {
                "\n_(dry run — no wiki writes)_"
            },
        )
    }
}

/// Build the wiki patch for a connection. Pure — unit-tested without IO.
/// `today` is the ISO date string for the `Connected:` / `## Source` line.
pub fn connection_patch(c: &Connection, today: &str) -> PersonPatch {
    let mut p = PersonPatch::new()
        .with_display_name(c.full_name())
        .identity("linkedin", &c.public_identifier)
        .profile_row("LinkedIn URL", c.profile_url())
        .source(format!(
            "Imported from LinkedIn 1st-degree connections on {today}"
        ));
    // Role: the headline verbatim is the most faithful self-reported title.
    if !c.headline.trim().is_empty() {
        p = p.profile_row("Role", c.headline.trim());
    }
    if !c.company.trim().is_empty() {
        p = p.profile_row("Company", c.company.trim());
    }
    if c.connected_at_ms > 0 {
        p = p.profile_row("Connected", iso_date(c.connected_at_ms));
    }
    p
}

fn iso_date(ms: i64) -> String {
    use time::{format_description, OffsetDateTime};
    let fmt = match format_description::parse("[year]-[month]-[day]") {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    OffsetDateTime::from_unix_timestamp(ms / 1000)
        .ok()
        .and_then(|dt| dt.date().format(&fmt).ok())
        .unwrap_or_default()
}

/// Slug for a connection's wiki page. Reuses the wiki's email-slug scheme
/// (`slug_from_email` — `@`→`_at_`, lowercased) fed a synthetic
/// `<public_identifier>@linkedin` so the slug space is shared with the
/// rest of the wiki and dedup is deterministic.
pub fn connection_slug(c: &Connection) -> String {
    slug_from_email(&format!("{}@linkedin", c.public_identifier))
}

/// Drive a full or delta sync. Pure orchestration over the injected
/// [`ConnectionsApi`] + a `read_page`/`write_page` closure pair so the wiki
/// IO stays mockable and the rate-ceiling logic is unit-testable.
///
/// `sleep` is injected (real `tokio::time::sleep` in prod, a no-op in tests)
/// so the 5 s page gap doesn't make the suite slow.
pub struct ConnectionSyncer<'a> {
    pub api: &'a dyn ConnectionsApi,
    pub layout: &'a WikiLayout,
    pub today: String,
    pub apply: bool,
}

impl<'a> ConnectionSyncer<'a> {
    pub async fn run<S, Fut>(
        &self,
        mode: SyncMode,
        start_offset: usize,
        mut sleep: S,
    ) -> Result<SyncReport, LinkedInError>
    where
        S: FnMut(Duration) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let mut report = SyncReport {
            mode: match mode {
                SyncMode::Full => "full".into(),
                SyncMode::Delta { .. } => "delta".into(),
            },
            applied: self.apply,
            ..Default::default()
        };

        let mut start = start_offset;
        let mut backoff_strikes: u32 = 0;
        let mut seen_slugs: BTreeSet<String> = BTreeSet::new();

        for page_no in 0..MAX_PAGES_PER_RUN {
            if page_no > 0 {
                sleep(MIN_PAGE_GAP).await;
            }
            let page = match self.api.fetch_page(start).await {
                Ok(p) => {
                    backoff_strikes = 0;
                    p
                }
                Err(LinkedInError::Voyager { status: 429, .. }) => {
                    backoff_strikes += 1;
                    let wait = BACKOFF_BASE * 2u32.saturating_pow(backoff_strikes - 1);
                    let jitter = Duration::from_millis((start as u64 * 137) % 5000);
                    warn!(
                        strikes = backoff_strikes,
                        wait_s = wait.as_secs(),
                        "linkedin connections 429; backing off"
                    );
                    if backoff_strikes > 5 {
                        return Err(LinkedInError::Voyager {
                            status: 429,
                            body: "exceeded 429 backoff ceiling".into(),
                        });
                    }
                    sleep(wait + jitter).await;
                    continue; // retry same `start`
                }
                Err(e) => return Err(e),
            };

            report.pages_fetched += 1;
            if page.is_empty() {
                break;
            }

            let mut stop_after = false;
            for conn in &page {
                // Delta: Voyager is recency-descending — first connection
                // older than the last full sync means everything past here
                // was already ingested.
                if let SyncMode::Delta { last_full_sync_ms } = mode {
                    if conn.connected_at_ms != 0
                        && conn.connected_at_ms < last_full_sync_ms
                    {
                        stop_after = true;
                        break;
                    }
                }
                report.connections_seen += 1;
                let slug = connection_slug(conn);
                if !seen_slugs.insert(slug.clone()) {
                    continue; // de-dupe within one run
                }
                let diff = self.apply_one(conn, &slug)?;
                match diff.action.as_str() {
                    "create" => report.created += 1,
                    "update" => report.updated += 1,
                    _ => report.noop += 1,
                }
                report.diffs.push(diff);
            }

            if stop_after {
                info!("delta sync reached last-full-sync boundary; stopping pager");
                break;
            }
            start += PAGE_SIZE;
        }

        Ok(report)
    }

    /// Read existing page (if any), merge fill-blanks-only, optionally write.
    fn apply_one(
        &self,
        conn: &Connection,
        slug: &str,
    ) -> Result<ConnectionDiff, LinkedInError> {
        let path: PathBuf = self
            .layout
            .people_dir()
            .join(format!("{slug}.md"));
        let existing = std::fs::read_to_string(&path).ok();
        let patch = connection_patch(conn, &self.today);
        let merged = merge_person_page(existing.as_deref(), &patch);

        let action = if !merged.changed {
            "noop"
        } else if merged.created {
            "create"
        } else {
            "update"
        };

        if self.apply && merged.changed {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| LinkedInError::Config(format!("mkdir: {e}")))?;
            }
            std::fs::write(&path, &merged.content)
                .map_err(|e| LinkedInError::Config(format!("write {slug}: {e}")))?;
        }

        Ok(ConnectionDiff {
            public_identifier: conn.public_identifier.clone(),
            name: conn.full_name(),
            slug: slug.to_string(),
            action: action.to_string(),
            filled: merged.filled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn conn(pi: &str, created: i64) -> Connection {
        Connection {
            first_name: "Jane".into(),
            last_name: "Doe".into(),
            public_identifier: pi.into(),
            headline: "Staff Engineer at Acme".into(),
            company: "Acme".into(),
            connected_at_ms: created,
        }
    }

    struct FakeApi {
        pages: Mutex<Vec<Vec<Connection>>>,
    }
    #[async_trait]
    impl ConnectionsApi for FakeApi {
        async fn fetch_page(&self, start: usize) -> Result<Vec<Connection>, LinkedInError> {
            let idx = start / PAGE_SIZE;
            let pages = self.pages.lock().unwrap();
            Ok(pages.get(idx).cloned().unwrap_or_default())
        }
    }

    fn layout() -> (tempfile::TempDir, WikiLayout) {
        let d = tempfile::TempDir::new().unwrap();
        let l = WikiLayout::new(d.path().to_path_buf());
        std::fs::create_dir_all(l.people_dir()).unwrap();
        (d, l)
    }

    async fn noop_sleep(_: Duration) {}

    #[test]
    fn parses_voyager_payload() {
        let v = serde_json::json!({
            "elements": [
                {
                    "createdAt": 1_700_000_000_000i64,
                    "connectedMemberResolutionResult": {
                        "firstName": "Jane",
                        "lastName": "Doe",
                        "publicIdentifier": "jane-doe-1",
                        "headline": "Staff Engineer at Acme"
                    }
                },
                { "createdAt": 0, "miniProfile": { "publicIdentifier": "" } }
            ]
        });
        let cs = parse_connections(&v);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].public_identifier, "jane-doe-1");
        assert_eq!(cs[0].company, "Acme");
        assert_eq!(cs[0].connected_at_ms, 1_700_000_000_000);
    }

    #[test]
    fn sync_mode_decision() {
        assert_eq!(SyncMode::decide(None, 100), SyncMode::Full);
        assert_eq!(
            SyncMode::decide(Some(0), FULL_SYNC_INTERVAL_MS + 1),
            SyncMode::Full
        );
        assert_eq!(
            SyncMode::decide(Some(1_000), 2_000),
            SyncMode::Delta {
                last_full_sync_ms: 1_000
            }
        );
    }

    #[test]
    fn patch_never_invents_blank_company() {
        let mut c = conn("x", 0);
        c.headline = "Independent".into(); // no " at "
        c.company = String::new();
        let p = connection_patch(&c, "2026-05-18");
        assert!(!p.profile.iter().any(|(k, _)| k == "Company"));
        assert!(p.profile.iter().any(|(k, _)| k == "Role"));
    }

    #[tokio::test]
    async fn dry_run_creates_nothing_on_disk() {
        let (_d, l) = layout();
        let api = FakeApi {
            pages: Mutex::new(vec![vec![conn("jane-1", 1_700_000_000_000)]]),
        };
        let s = ConnectionSyncer {
            api: &api,
            layout: &l,
            today: "2026-05-18".into(),
            apply: false,
        };
        let r = s.run(SyncMode::Full, 0, noop_sleep).await.unwrap();
        assert_eq!(r.created, 1);
        assert!(!r.applied);
        // Nothing written.
        assert!(std::fs::read_dir(l.people_dir()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn apply_writes_and_is_idempotent() {
        let (_d, l) = layout();
        let api = FakeApi {
            pages: Mutex::new(vec![vec![conn("jane-2", 1_700_000_000_000)]]),
        };
        let s = ConnectionSyncer {
            api: &api,
            layout: &l,
            today: "2026-05-18".into(),
            apply: true,
        };
        let r1 = s.run(SyncMode::Full, 0, noop_sleep).await.unwrap();
        assert_eq!(r1.created, 1);
        let page = l.people_dir().join(format!(
            "{}.md",
            connection_slug(&conn("jane-2", 0))
        ));
        assert!(page.is_file());
        let body = std::fs::read_to_string(&page).unwrap();
        assert!(body.contains("linkedin: jane-2"));
        assert!(body.contains("- **Role:** Staff Engineer at Acme"));

        // Second run: fill-blanks-only → all noop.
        let r2 = s.run(SyncMode::Full, 0, noop_sleep).await.unwrap();
        assert_eq!(r2.created, 0);
        assert_eq!(r2.noop, 1);
        assert_eq!(std::fs::read_to_string(&page).unwrap(), body);
    }

    #[tokio::test]
    async fn delta_stops_at_last_full_sync_boundary() {
        let (_d, l) = layout();
        // Page is recency-descending: newest first.
        let api = FakeApi {
            pages: Mutex::new(vec![vec![
                conn("new-1", 2_000),
                conn("new-2", 1_500),
                conn("old-1", 500), // predates boundary 1_000 → stop here
                conn("old-2", 100),
            ]]),
        };
        let s = ConnectionSyncer {
            api: &api,
            layout: &l,
            today: "2026-05-18".into(),
            apply: false,
        };
        let r = s
            .run(
                SyncMode::Delta {
                    last_full_sync_ms: 1_000,
                },
                0,
                noop_sleep,
            )
            .await
            .unwrap();
        assert_eq!(r.connections_seen, 2, "only the two newer than boundary");
    }

    #[tokio::test]
    async fn pager_walks_until_empty_page() {
        let (_d, l) = layout();
        let api = FakeApi {
            pages: Mutex::new(vec![
                vec![conn("a", 9), conn("b", 8)],
                vec![conn("c", 7)],
                vec![], // terminator
            ]),
        };
        let s = ConnectionSyncer {
            api: &api,
            layout: &l,
            today: "2026-05-18".into(),
            apply: false,
        };
        let r = s.run(SyncMode::Full, 0, noop_sleep).await.unwrap();
        assert_eq!(r.connections_seen, 3);
        assert_eq!(r.pages_fetched, 3);
    }

    #[test]
    fn discord_summary_marks_dry_run() {
        let r = SyncReport {
            mode: "delta".into(),
            created: 3,
            updated: 2,
            noop: 10,
            applied: false,
            ..Default::default()
        };
        let s = r.discord_summary();
        assert!(s.contains("3 new"));
        assert!(s.contains("dry run"));
    }
}
