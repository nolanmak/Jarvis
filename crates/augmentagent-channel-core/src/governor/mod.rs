//! Outbound-action rate governance for every per-platform channel (#83).
//!
//! Sits between a channel's intent ("post this", "like that", "send connection
//! request") and the actual platform call. Two responsibilities:
//!
//! 1. [`RateGovernor::permit`] — refuse / delay an action if it would breach
//!    the per-platform sliding-window cap, the warmup multiplier, the
//!    quiet-hours window, or a circuit-breaker halt.
//! 2. [`RateGovernor::record`] — persist outcome (ok / failed / suspicion) so
//!    future `permit()` calls reflect reality after restart.
//!
//! All state is in SQLite (`rate_events`, `rate_halts`, `rate_warmup`).
//! Single source of truth, survives restart, queryable for ban-investigation.
//!
//! ### Wiring (channels adopt in their own PRs — see #83 §9)
//!
//! ```ignore
//! let req = ActionRequest {
//!     platform: Platform::LinkedIn,
//!     action:   ActionKind::ConnectionInvite,
//!     account_id: self.account.urn.clone(),
//!     risk:     classify_target_risk(&self.wiki, target_urn),
//!     cause:    format!("invite:{target_urn}"),
//!     target_id: Some(target_urn.into()),
//!     target_attrs: Some(TargetAttrs { known_contact: false, ..Default::default() }),
//! };
//! let permit = match self.governor.permit(req).await {
//!     Ok(p) => p,
//!     Err(Denial::ApprovalRequired { .. }) => return self.queue_approval(...).await,
//!     Err(d) => { tracing::warn!(?d, "denied"); return Err(d.into()); }
//! };
//! tokio::time::sleep(next_action_delay(min_gap, now_ms)).await; // §5 jitter, BEFORE the call
//! let outcome = match self.api.do_thing().await {
//!     Ok(_)  => Outcome::Ok,
//!     Err(e) if is_suspicion(&e) => Outcome::Suspicion,
//!     Err(_) => Outcome::Failed,
//! };
//! self.governor.record(permit, outcome).await?;
//! ```

pub mod limits;
pub mod windowed_counter;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Datelike, Local, NaiveTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use augmentagent_store::Store;

pub use limits::{lookup as lookup_limit, RateCaps, RateLimit, RATE_TABLE};
pub use windowed_counter::WindowedCounter;

// =============================================================================
// Public types
// =============================================================================

/// Platforms the governor knows about. Future-proofed for TikTok / Bluesky;
/// the cap matrix only carries rows for the three currently-implemented ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Instagram,
    LinkedIn,
    Twitter,
    /// Reserved — no rows in [`RATE_TABLE`] yet.
    TikTok,
    /// Reserved — no rows in [`RATE_TABLE`] yet.
    Bluesky,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instagram => "instagram",
            Self::LinkedIn => "linkedin",
            Self::Twitter => "twitter",
            Self::TikTok => "tiktok",
            Self::Bluesky => "bluesky",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "instagram" => Some(Self::Instagram),
            "linkedin" => Some(Self::LinkedIn),
            "twitter" | "x" => Some(Self::Twitter),
            "tiktok" => Some(Self::TikTok),
            "bluesky" => Some(Self::Bluesky),
            _ => None,
        }
    }
}

/// Categories of outbound action a channel can take. Drives the cap-matrix
/// lookup and the approval-required matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Post,
    Like,
    Comment,
    Reply,
    Follow,
    Unfollow,
    Dm,
    ConnectionInvite,
    ProfileView,
    Repost,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Like => "like",
            Self::Comment => "comment",
            Self::Reply => "reply",
            Self::Follow => "follow",
            Self::Unfollow => "unfollow",
            Self::Dm => "dm",
            Self::ConnectionInvite => "connection_invite",
            Self::ProfileView => "profile_view",
            Self::Repost => "repost",
        }
    }
}

/// Caller-supplied risk hint. Channels classify the target (in-wiki contact
/// vs. stranger, mass-action batch, etc.) before calling `permit()` so the
/// governor's approval matrix stays a pure function of `(action, risk)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// Categorical attributes about the target of an action — fed into
/// [`requires_approval`] to decide whether the action needs human review
/// regardless of cap headroom.
///
/// Channels populate these from their own world: wiki lookups, follower
/// graphs, prior-DM history. Defaults are deliberately conservative
/// ("stranger", no prior contact) so an absent flag escalates rather than
/// silently bypasses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetAttrs {
    /// Target appears in the user's wiki contacts / 1st-degree network.
    pub known_contact: bool,
    /// Action is part of a batch ≥ N similar actions in a short window.
    pub mass_action: bool,
    /// Channel has detected this is a brand-new conversation thread (cold DM,
    /// stranger comment, etc.). Drives stricter approval matrix entries.
    pub stranger: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRequest {
    pub platform: Platform,
    pub action: ActionKind,
    pub account_id: String,
    pub risk: Risk,
    pub cause: String,
    pub target_id: Option<String>,
    /// Optional target classification. `None` is treated as a stranger.
    #[serde(default)]
    pub target_attrs: Option<TargetAttrs>,
}

/// Returned by [`RateGovernor::permit`] and consumed by
/// [`RateGovernor::record`]. Carries everything the matching `record()` call
/// needs to write a complete row without re-deriving it.
#[derive(Debug, Clone)]
pub struct Permit {
    pub id: Uuid,
    pub req: ActionRequest,
    pub reserved_at_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum Denial {
    #[error("daily cap reached for {platform:?}/{action:?}: {used}/{cap}")]
    DailyCap {
        platform: Platform,
        action: ActionKind,
        used: u32,
        cap: u32,
    },
    #[error("hourly cap reached for {platform:?}/{action:?}: {used}/{cap}")]
    HourlyCap {
        platform: Platform,
        action: ActionKind,
        used: u32,
        cap: u32,
    },
    #[error("burst cap reached for {platform:?}/{action:?} in 5min window: {used}/{cap}")]
    BurstCap {
        platform: Platform,
        action: ActionKind,
        used: u32,
        cap: u32,
    },
    #[error("min-gap violated for {platform:?}/{action:?}; next allowed in {next_in:?}")]
    MinGap {
        platform: Platform,
        action: ActionKind,
        next_in: Duration,
    },
    #[error("quiet hours active until {until_ms}")]
    QuietHours { until_ms: i64 },
    #[error("warmup not ready (multiplier {0:.2} would deny)")]
    WarmupGate(f64),
    #[error("circuit breaker open until {until_ms}: {reason}")]
    Halted { until_ms: i64, reason: String },
    #[error("manual approval required (risk={risk:?}, action={action:?})")]
    ApprovalRequired { risk: Risk, action: ActionKind },
    #[error("storage error: {0}")]
    Storage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    /// Action attempted, platform returned an error — still costs quota.
    Failed,
    /// Never executed (network blew up before send) — refund quota.
    RolledBack,
    /// Captcha / blocked toast / login challenge — trips circuit breaker.
    Suspicion,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
            Self::Suspicion => "suspicion",
        }
    }
}

/// Reason a halt was opened. Channels classify their own raw signals
/// (HTTP status, DOM toast text, redirect URL) into one of these before
/// calling [`RateGovernor::record_halt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    ActionBlocked,
    Captcha,
    LoginChallenge,
    RateLimitToast,
}

impl HaltReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActionBlocked => "action_blocked",
            Self::Captcha => "captcha",
            Self::LoginChallenge => "login_challenge",
            Self::RateLimitToast => "rate_limit_toast",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HaltState {
    pub platform: Platform,
    pub paused_until_ms: i64,
    pub reason: String,
    /// String form of the triggering event id (uuid). Stored as `String` so
    /// `HaltState` is `Serialize` without requiring the `serde` feature on
    /// the workspace `uuid` dep — the field is still parseable via
    /// `Uuid::parse_str` if a caller cares.
    pub triggered_by_event_id: Option<String>,
}

// =============================================================================
// Trait
// =============================================================================

#[async_trait]
pub trait RateGovernor: Send + Sync {
    async fn permit(&self, action: ActionRequest) -> Result<Permit, Denial>;
    async fn record(&self, permit: Permit, outcome: Outcome) -> anyhow::Result<()>;
    /// Open a circuit-breaker halt for `platform` (separate from `record()`
    /// because not every halt is tied to a specific permit — e.g. a session
    /// invalidation observed during a poll loop).
    async fn record_halt(
        &self,
        platform: Platform,
        reason: HaltReason,
        paused_until_ms: i64,
    ) -> anyhow::Result<()>;
    /// Inspect halt state without consuming a permit (dashboard / Discord card).
    async fn halt_status(&self, p: Platform) -> Option<HaltState>;
    /// Return paused-until ms iff the platform is currently halted; `None`
    /// when the halt has expired or never existed. Equivalent to
    /// `halt_status().filter(|h| h.paused_until_ms > now())`.
    async fn is_halted(&self, p: Platform) -> Option<i64>;
}

// =============================================================================
// Clock abstraction (so tests don't sleep)
// =============================================================================

/// Injectable wall-clock so tests can pin / advance time without sleeping.
/// Production uses [`SystemClock`].
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    /// Local hour (0-23) for quiet-hours math. Implementations decide
    /// what "local" means — the system one returns the host's local TZ.
    fn local_hour(&self) -> u32;
}

#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn local_hour(&self) -> u32 {
        Local::now().hour()
    }
}

// =============================================================================
// Warmup curve (#83 §4)
// =============================================================================

/// 4-week ramp: 25% → 50% → 75% → 90% → 100%. Schedule cribbed from the
/// Expandi LinkedIn warm-up guide and instagrapi's "increase volume slowly
/// over days, not minutes" guidance.
///
/// Pure function of `now - started`. Burst caps and min_gap are NOT scaled
/// by this multiplier — those are anti-pattern guards, not volume controls.
pub fn warmup_curve(now_ms: i64, started_at_ms: i64) -> f64 {
    let days = (now_ms - started_at_ms).max(0) / 86_400_000;
    match days {
        0..=6 => 0.25,
        7..=13 => 0.50,
        14..=20 => 0.75,
        21..=27 => 0.90,
        _ => 1.00,
    }
}

/// Apply warmup multiplier to a cap. Floors at 1 so a Risk::Low Like isn't
/// wholly impossible on day 1.
pub fn scale_cap(cap: u32, multiplier: f64) -> u32 {
    let scaled = (cap as f64 * multiplier).floor() as u32;
    scaled.max(1)
}

// =============================================================================
// Quiet hours + jitter (#83 §5)
// =============================================================================

/// Quiet hours per #83 §5 — no actions between 02:00 and 06:00 local.
/// `clock.local_hour()` is consulted; when the hour is in the quiet window,
/// returns `Some(unix_ms_at_next_06_00_local)` so the caller can surface a
/// useful "try again at" timestamp.
pub fn quiet_hours_until(clock: &dyn Clock) -> Option<i64> {
    let h = clock.local_hour();
    if !(2..6).contains(&h) {
        return None;
    }
    // Compute next 06:00 local in unix-ms. Use chrono Local for the date
    // component — clock.local_hour is just a convenience that doesn't carry
    // the date.
    let now = Local::now();
    let today_six = NaiveTime::from_hms_opt(6, 0, 0).unwrap();
    let target = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 6, 0, 0)
        .single()
        .or_else(|| {
            // Fallback for unusual TZ shifts: build via NaiveDateTime.
            now.date_naive()
                .and_time(today_six)
                .and_local_timezone(Local)
                .single()
        })?;
    Some(target.timestamp_millis())
}

/// Time-of-day jitter (#83 §5 layer B). Returns a `Duration` to sleep
/// *before* executing the action, sampled from a log-normal distribution
/// centered on `min_gap` with a long right tail to break the
/// "perfectly-uniform interval" pattern flagged in
/// Buffer / Phantombuster / instagrapi guides.
///
/// Implementation note: we use a deterministic splitmix64-derived PRNG
/// seeded from `now_ms` so we don't need to add `rand` to the workspace
/// just for this helper. The distribution is good-enough for traffic-
/// shaping; we're not generating cryptographic keys.
pub fn next_action_delay(min_gap: Duration, now_ms: i64) -> Duration {
    if min_gap.is_zero() {
        return Duration::ZERO;
    }
    // mu = ln(min_gap_secs); sigma = 0.6 → ~95% of samples in
    // [min_gap, 3.5 * min_gap]; mean ≈ 1.4 * min_gap.
    let min_secs = min_gap.as_secs_f64().max(1.0);
    let mu = min_secs.ln();
    let sigma = 0.6_f64;
    // Box-Muller from two uniforms.
    let (u1, u2) = (uniform(now_ms, 1), uniform(now_ms, 2));
    let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
    let sample = (mu + sigma * z).exp();
    // Clamp to [min_gap, 4 * min_gap] so we never *under*-shoot the gap.
    let clamped = sample.clamp(min_secs, 4.0 * min_secs);
    Duration::from_secs_f64(clamped)
}

/// Cheap deterministic uniform-(0,1) sample from a u64 seed. Splitmix64
/// scaled into the open interval. `salt` lets one seed yield independent
/// pairs.
fn uniform(seed: i64, salt: u64) -> f64 {
    let mut z = (seed as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(salt);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Map to (0, 1) — avoid 0 exactly so ln() is well-defined.
    let f = (z >> 11) as f64 / ((1u64 << 53) as f64);
    if f <= 0.0 {
        f64::EPSILON
    } else if f >= 1.0 {
        1.0 - f64::EPSILON
    } else {
        f
    }
}

// =============================================================================
// Approval-required matrix (#83 §7)
// =============================================================================

/// Pure function: does this action require human approval, regardless of
/// cap headroom? Channels route a `true` return through the existing
/// approval-card flow.
///
/// Matrix per #83 §7:
/// - Like / Repost: auto-OK (cheapest social action)
/// - Unfollow / ProfileView: auto-OK
/// - Comment / Reply / Post: always approval (anything text-generative)
/// - ConnectionInvite: always approval
/// - Follow on stranger or High-risk: approval; on known contact: auto-OK
/// - DM (any): approval (cold-DM ban risk dominates)
/// - mass_action == true on any kind: approval (one batch card)
pub fn requires_approval(req: &ActionRequest) -> bool {
    use ActionKind::*;
    let attrs = req.target_attrs.unwrap_or_default();
    if attrs.mass_action {
        // Mass-action batches always get a single batch-approval card per #83 §7.
        return true;
    }
    match req.action {
        Like | Repost | Unfollow | ProfileView => false,
        Comment | Reply | Post | ConnectionInvite | Dm => true,
        Follow => {
            // Only auto-permit Follow when the target is a known contact AND risk is Low.
            !(attrs.known_contact && req.risk == Risk::Low)
        }
    }
}

// =============================================================================
// SqliteGovernor impl
// =============================================================================

/// Production [`RateGovernor`] backed by the shared `data.db`. Cheap to
/// `Arc::clone` — the only state inside is the store + clock pointers.
pub struct SqliteGovernor {
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
}

impl SqliteGovernor {
    pub fn new(store: Arc<Store>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    pub fn with_system_clock(store: Arc<Store>) -> Self {
        Self::new(store, Arc::new(SystemClock))
    }

    /// Return the warmup multiplier for a (platform, account) pair, seeding
    /// the row at `now` if it doesn't exist yet.
    pub fn warmup_multiplier(
        &self,
        platform: Platform,
        account_id: &str,
        now_ms: i64,
    ) -> Result<f64, Denial> {
        let existing = self
            .store
            .rate_get_warmup(platform.as_str(), account_id)
            .map_err(|e| Denial::Storage(e.to_string()))?;
        let started = match existing {
            Some(w) => w.warmup_started_at_ms,
            None => {
                self.store
                    .rate_seed_warmup(platform.as_str(), account_id, now_ms)
                    .map_err(|e| Denial::Storage(e.to_string()))?;
                now_ms
            }
        };
        Ok(warmup_curve(now_ms, started))
    }

    /// Internal: actually run the cap math. Split out so unit tests can
    /// poke individual gates without going through the trait.
    fn evaluate(&self, req: &ActionRequest) -> Result<(), Denial> {
        let now_ms = self.clock.now_ms();

        // 0. Circuit breaker takes precedence over everything else.
        if let Some(h) = self
            .store
            .rate_halt_state(req.platform.as_str())
            .map_err(|e| Denial::Storage(e.to_string()))?
        {
            if h.paused_until_ms > now_ms {
                return Err(Denial::Halted {
                    until_ms: h.paused_until_ms,
                    reason: h.reason,
                });
            }
        }

        // 1. Approval-required matrix (returns Denial::ApprovalRequired so the
        //    channel can route to the approval-card flow rather than treat it
        //    as an error).
        if requires_approval(req) {
            return Err(Denial::ApprovalRequired {
                risk: req.risk,
                action: req.action,
            });
        }

        // 2. Quiet hours.
        if let Some(until) = quiet_hours_until(self.clock.as_ref()) {
            return Err(Denial::QuietHours { until_ms: until });
        }

        // 3. Look up caps; absence of a row means "no opinion, allow".
        let Some(limit) = lookup_limit(req.platform, req.action) else {
            return Ok(());
        };
        let multiplier = self.warmup_multiplier(req.platform, &req.account_id, now_ms)?;
        let counter = WindowedCounter::new(
            &self.store,
            req.platform,
            req.action,
            &req.account_id,
        );

        // 4. min_gap (NOT scaled by warmup — anti-pattern guard).
        if !limit.min_gap.is_zero() {
            if let Some(last_ms) = counter
                .last_event_at()
                .map_err(|e| Denial::Storage(e.to_string()))?
            {
                let elapsed_ms = (now_ms - last_ms).max(0);
                let min_ms = limit.min_gap.as_millis() as i64;
                if elapsed_ms < min_ms {
                    return Err(Denial::MinGap {
                        platform: req.platform,
                        action: req.action,
                        next_in: Duration::from_millis((min_ms - elapsed_ms) as u64),
                    });
                }
            }
        }

        // 5. burst (5min) — strictest, NOT scaled.
        if let Some(burst_cap) = limit.burst_5m {
            let used = counter
                .count_in_window(now_ms, Duration::from_secs(300))
                .map_err(|e| Denial::Storage(e.to_string()))?;
            if used >= burst_cap {
                return Err(Denial::BurstCap {
                    platform: req.platform,
                    action: req.action,
                    used,
                    cap: burst_cap,
                });
            }
        }

        // 6. hour, scaled by warmup.
        if let Some(hour_cap) = limit.hour {
            let scaled = scale_cap(hour_cap, multiplier);
            let used = counter
                .count_in_window(now_ms, Duration::from_secs(3600))
                .map_err(|e| Denial::Storage(e.to_string()))?;
            if used >= scaled {
                return Err(Denial::HourlyCap {
                    platform: req.platform,
                    action: req.action,
                    used,
                    cap: scaled,
                });
            }
        }

        // 7. day, scaled by warmup.
        let day_scaled = scale_cap(limit.day, multiplier);
        let used = counter
            .count_in_window(now_ms, Duration::from_secs(86_400))
            .map_err(|e| Denial::Storage(e.to_string()))?;
        if used >= day_scaled {
            return Err(Denial::DailyCap {
                platform: req.platform,
                action: req.action,
                used,
                cap: day_scaled,
            });
        }

        Ok(())
    }
}

#[async_trait]
impl RateGovernor for SqliteGovernor {
    async fn permit(&self, req: ActionRequest) -> Result<Permit, Denial> {
        self.evaluate(&req)?;
        Ok(Permit {
            id: Uuid::new_v4(),
            reserved_at_ms: self.clock.now_ms(),
            req,
        })
    }

    async fn record(&self, permit: Permit, outcome: Outcome) -> anyhow::Result<()> {
        self.store
            .insert_rate_event(
                &permit.id.to_string(),
                permit.req.platform.as_str(),
                permit.req.action.as_str(),
                &permit.req.account_id,
                permit.reserved_at_ms,
                outcome.as_str(),
                &permit.req.cause,
                permit.req.target_id.as_deref(),
                None,
            )
            .map_err(|e| anyhow::anyhow!(e))?;
        if outcome == Outcome::Suspicion {
            // Default policy per #83 §6: suspicion → 24h halt. Channels can
            // call record_halt() directly with a tighter window if they want
            // graduated handling (e.g. 1h for the first 429, exponential
            // back-off thereafter).
            let now_ms = self.clock.now_ms();
            let id_str = permit.id.to_string();
            self.store
                .rate_set_halt(
                    permit.req.platform.as_str(),
                    now_ms + 24 * 3600 * 1000,
                    "suspicion signal observed",
                    Some(&id_str),
                    now_ms,
                )
                .map_err(|e| anyhow::anyhow!(e))?;
        }
        Ok(())
    }

    async fn record_halt(
        &self,
        platform: Platform,
        reason: HaltReason,
        paused_until_ms: i64,
    ) -> anyhow::Result<()> {
        let now_ms = self.clock.now_ms();
        self.store
            .rate_set_halt(
                platform.as_str(),
                paused_until_ms,
                reason.as_str(),
                None,
                now_ms,
            )
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    async fn halt_status(&self, p: Platform) -> Option<HaltState> {
        let row = self.store.rate_halt_state(p.as_str()).ok().flatten()?;
        Some(HaltState {
            platform: p,
            paused_until_ms: row.paused_until_ms,
            reason: row.reason,
            triggered_by_event_id: row.triggered_by_event_id,
        })
    }

    async fn is_halted(&self, p: Platform) -> Option<i64> {
        let h = self.halt_status(p).await?;
        if h.paused_until_ms > self.clock.now_ms() {
            Some(h.paused_until_ms)
        } else {
            None
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use tempfile::TempDir;

    /// Pin-in-place clock for tests: fixed `now_ms` + fixed local hour.
    pub(crate) struct FakeClock {
        now: AtomicI64,
        hour: AtomicI64,
    }

    impl FakeClock {
        pub(crate) fn new(now: i64, hour: u32) -> Self {
            Self {
                now: AtomicI64::new(now),
                hour: AtomicI64::new(hour as i64),
            }
        }
        pub(crate) fn advance(&self, d: Duration) {
            self.now.fetch_add(d.as_millis() as i64, Ordering::SeqCst);
        }
        pub(crate) fn set_hour(&self, h: u32) {
            self.hour.store(h as i64, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            self.now.load(Ordering::SeqCst)
        }
        fn local_hour(&self) -> u32 {
            self.hour.load(Ordering::SeqCst) as u32
        }
    }

    fn fresh_store() -> (Arc<Store>, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rg.db");
        // Store::open runs additive migrations against tables the Node tree
        // ordinarily owns (`actions`, `emails`, `gmail_accounts`,
        // `channel_subscriptions`, `slack_workspaces`). Seed the minimal
        // shape they expect before opening so the migration is a no-op.
        seed_node_owned_tables(&path);
        let store = Store::open(&path).unwrap();
        (Arc::new(store), dir)
    }

    /// Mirrors the schema the Node `src/db.ts` writes on first boot, just
    /// enough to satisfy `Store::migrate`'s `column_exists` probes.
    pub(crate) fn seed_node_owned_tables(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
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

    fn req(p: Platform, a: ActionKind, account: &str) -> ActionRequest {
        ActionRequest {
            platform: p,
            action: a,
            account_id: account.into(),
            risk: Risk::Low,
            cause: "test".into(),
            target_id: None,
            target_attrs: Some(TargetAttrs {
                known_contact: true, // suppress approval gate for cap tests
                mass_action: false,
                stranger: false,
            }),
        }
    }

    #[test]
    fn warmup_curve_breakpoints() {
        let s = 0_i64;
        let day = 86_400_000_i64;
        assert_eq!(warmup_curve(s, s), 0.25);
        assert_eq!(warmup_curve(s + 6 * day, s), 0.25);
        assert_eq!(warmup_curve(s + 7 * day, s), 0.50);
        assert_eq!(warmup_curve(s + 13 * day, s), 0.50);
        assert_eq!(warmup_curve(s + 14 * day, s), 0.75);
        assert_eq!(warmup_curve(s + 20 * day, s), 0.75);
        assert_eq!(warmup_curve(s + 21 * day, s), 0.90);
        assert_eq!(warmup_curve(s + 27 * day, s), 0.90);
        assert_eq!(warmup_curve(s + 28 * day, s), 1.00);
    }

    #[test]
    fn scale_cap_floors_at_one() {
        assert_eq!(scale_cap(60, 0.25), 15);
        assert_eq!(scale_cap(2, 0.25), 1); // would be 0; floored to 1
        assert_eq!(scale_cap(0, 1.0), 1); // even 0 cap floors to 1
    }

    #[test]
    fn requires_approval_matrix_rows() {
        let mut r = req(Platform::Instagram, ActionKind::Like, "acct");
        assert!(!requires_approval(&r));
        r.action = ActionKind::Comment;
        assert!(requires_approval(&r));
        r.action = ActionKind::Post;
        assert!(requires_approval(&r));
        r.action = ActionKind::ConnectionInvite;
        assert!(requires_approval(&r));
        // Follow + known contact + Low risk → auto-permit
        r.action = ActionKind::Follow;
        r.target_attrs = Some(TargetAttrs {
            known_contact: true,
            mass_action: false,
            stranger: false,
        });
        assert!(!requires_approval(&r));
        // Follow + stranger → approval
        r.target_attrs = Some(TargetAttrs::default());
        assert!(requires_approval(&r));
        // Mass action escalates everything to approval, even Like
        let mut r2 = req(Platform::Instagram, ActionKind::Like, "acct");
        r2.target_attrs = Some(TargetAttrs {
            known_contact: true,
            mass_action: true,
            stranger: false,
        });
        assert!(requires_approval(&r2));
    }

    #[tokio::test]
    async fn cap_holds_then_clears_after_window_slide() {
        let (store, _dir) = fresh_store();
        let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));
        let gov = SqliteGovernor::new(store.clone(), clock.clone());
        // IG Like cap = 60 * 0.25 (day-1 warmup) = 15.
        let mut ok = 0u32;
        for _ in 0..50 {
            // Advance past min_gap so we don't hit the gap denial first.
            clock.advance(Duration::from_secs(31));
            let r = req(Platform::Instagram, ActionKind::Like, "ig_acct");
            match gov.permit(r).await {
                Ok(p) => {
                    gov.record(p, Outcome::Ok).await.unwrap();
                    ok += 1;
                }
                Err(Denial::DailyCap { .. })
                | Err(Denial::HourlyCap { .. })
                | Err(Denial::BurstCap { .. }) => break,
                Err(other) => panic!("unexpected denial: {other:?}"),
            }
        }
        assert!(
            ok > 0 && ok <= 15,
            "expected to land within day-1 warmup cap (≤15), got {ok}"
        );
        // Advance 24h+1s — sliding window should have forgotten everything,
        // and warmup multiplier hasn't advanced enough to flip.
        clock.advance(Duration::from_secs(86_400 + 1));
        let r = req(Platform::Instagram, ActionKind::Like, "ig_acct");
        let p = gov.permit(r).await.expect("post-window permit");
        gov.record(p, Outcome::Ok).await.unwrap();
    }

    #[tokio::test]
    async fn min_gap_enforced() {
        let (store, _dir) = fresh_store();
        // hour 12: outside quiet hours
        let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));
        let gov = SqliteGovernor::new(store.clone(), clock.clone());
        let r = req(Platform::Instagram, ActionKind::Like, "g");
        let p = gov.permit(r.clone()).await.unwrap();
        gov.record(p, Outcome::Ok).await.unwrap();
        // Immediately try again — min_gap is 30s for IG Like.
        match gov.permit(r.clone()).await {
            Err(Denial::MinGap { next_in, .. }) => {
                assert!(next_in.as_secs() <= 30, "next_in was {next_in:?}");
            }
            other => panic!("expected MinGap, got {other:?}"),
        }
        clock.advance(Duration::from_secs(31));
        let _ = gov.permit(r).await.expect("permit after gap clears");
    }

    #[tokio::test]
    async fn quiet_hours_block_during_window() {
        let (store, _dir) = fresh_store();
        let clock = Arc::new(FakeClock::new(1_700_000_000_000, 3)); // 03:00 local
        let gov = SqliteGovernor::new(store.clone(), clock.clone());
        let r = req(Platform::Instagram, ActionKind::Like, "g");
        match gov.permit(r.clone()).await {
            Err(Denial::QuietHours { .. }) => {}
            other => panic!("expected QuietHours, got {other:?}"),
        }
        clock.set_hour(7);
        let _ = gov.permit(r).await.expect("post-quiet permit");
    }

    #[tokio::test]
    async fn circuit_breaker_blocks_then_clears() {
        let (store, _dir) = fresh_store();
        let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));
        let gov = SqliteGovernor::new(store.clone(), clock.clone());
        gov.record_halt(
            Platform::Instagram,
            HaltReason::Captcha,
            clock.now_ms() + 60_000,
        )
        .await
        .unwrap();
        assert!(gov.is_halted(Platform::Instagram).await.is_some());
        let r = req(Platform::Instagram, ActionKind::Like, "g");
        match gov.permit(r.clone()).await {
            Err(Denial::Halted { .. }) => {}
            other => panic!("expected Halted, got {other:?}"),
        }
        clock.advance(Duration::from_secs(61));
        assert!(gov.is_halted(Platform::Instagram).await.is_none());
        let _ = gov.permit(r).await.expect("post-halt permit");
    }

    #[tokio::test]
    async fn rolled_back_does_not_burn_quota() {
        let (store, _dir) = fresh_store();
        let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));
        let gov = SqliteGovernor::new(store.clone(), clock.clone());
        // Burn 5 RolledBack permits — none of them should count.
        let r = req(Platform::Instagram, ActionKind::Like, "g");
        for _ in 0..5 {
            clock.advance(Duration::from_secs(31));
            let p = gov.permit(r.clone()).await.unwrap();
            gov.record(p, Outcome::RolledBack).await.unwrap();
        }
        let counter =
            WindowedCounter::new(&store, Platform::Instagram, ActionKind::Like, "g");
        let n = counter
            .count_in_window(clock.now_ms(), Duration::from_secs(86_400))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn suspicion_outcome_opens_halt() {
        let (store, _dir) = fresh_store();
        let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));
        let gov = SqliteGovernor::new(store.clone(), clock.clone());
        let r = req(Platform::Instagram, ActionKind::Like, "g");
        let p = gov.permit(r).await.unwrap();
        gov.record(p, Outcome::Suspicion).await.unwrap();
        let h = gov.halt_status(Platform::Instagram).await.unwrap();
        assert!(h.paused_until_ms > clock.now_ms());
    }

    #[tokio::test]
    async fn approval_required_for_invite() {
        let (store, _dir) = fresh_store();
        let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));
        let gov = SqliteGovernor::new(store, clock);
        let mut r = req(Platform::LinkedIn, ActionKind::ConnectionInvite, "li");
        r.target_attrs = Some(TargetAttrs::default());
        match gov.permit(r).await {
            Err(Denial::ApprovalRequired { .. }) => {}
            other => panic!("expected ApprovalRequired, got {other:?}"),
        }
    }

    #[test]
    fn next_action_delay_respects_min_gap() {
        let d = next_action_delay(Duration::from_secs(60), 12345);
        assert!(d >= Duration::from_secs(60));
        assert!(d <= Duration::from_secs(60 * 4));
        assert_eq!(next_action_delay(Duration::ZERO, 0), Duration::ZERO);
    }
}
