//! Structured ask-detection + auto-fill.
//!
//! **Phase 1** (merged) is telemetry only: a shadow extractor logs detected
//! asks; nothing is injected, no resolver runs.
//!
//! **Phase 2** (this module, #35): four *live* deterministic resolvers and a
//! draft-injection path. Gating is layered and conservative — this code path
//! costs a per-message Haiku call plus external API calls, so every stage is
//! behind an explicit env flag and the default is byte-identical to today.
//!
//! Gating model:
//! - `AUGMENTAGENT_ASK_RESOLVE` — `off` (default), `shadow`, or `live`.
//!   - `off`   ⇒ no extractor call, no resolvers, empty injection.
//!   - `shadow`⇒ extractor runs + logs telemetry, NO resolve, NO injection.
//!   - `live`  ⇒ extractor runs, resolvers run, `<resolved_asks>` injected.
//! - Each resolver ALSO has its own flag (e.g.
//!   `AUGMENTAGENT_ASK_RESOLVE_SCHEDULING=1`). A resolver whose flag is unset
//!   short-circuits to `Ok(None)` and makes zero network calls — so `live`
//!   with no per-resolver flags set is a safe no-op (empty injection ⇒
//!   byte-identical draft prompt).
//! - Confidence threshold ([`INJECT_CONFIDENCE_FLOOR`], default 0.7): asks
//!   below the floor are never resolved or injected.
//!
//! The intro resolver NEVER auto-executes — it only surfaces a suggestion
//! string for the drafter; sending an intro stays a human decision.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Datelike, Duration as ChronoDuration, Utc, Weekday};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::reasoner::{Reasoner, ReasonerOpts};

const ASK_EXTRACT_PROMPT: &str = include_str!("../prompts/ask-extract.md");

/// Total wall-clock budget for the parallel resolve stage. The issue caps
/// this at <3s; resolvers run concurrently under `tokio::join!` so the budget
/// is per-stage, not per-resolver.
pub const RESOLVE_BUDGET: Duration = Duration::from_secs(3);

/// Asks scoring below this are neither resolved nor injected. Issue default
/// is 0.7; override via `AUGMENTAGENT_ASK_RESOLVE_MIN_CONFIDENCE`.
pub const INJECT_CONFIDENCE_FLOOR: f64 = 0.7;

fn confidence_floor() -> f64 {
    std::env::var("AUGMENTAGENT_ASK_RESOLVE_MIN_CONFIDENCE")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|f| f.clamp(0.0, 1.0))
        .unwrap_or(INJECT_CONFIDENCE_FLOOR)
}

fn flag_on(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

/// Which deterministic resolver *would* handle an ask. Phase 1 only records
/// this; Phase 2 dispatches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolverKind {
    Scheduling,
    Calendly,
    ShareDoc,
    Intro,
    /// A request for a video-call join link (Zoom / Meet / Teams) — distinct
    /// from `calendly` (a *booking* page) and `scheduling` (proposing times).
    /// "Send me the Zoom", "what's the meeting link?".
    MeetingLink,
    None,
}

impl ResolverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduling => "scheduling",
            Self::Calendly => "calendly",
            Self::ShareDoc => "share_doc",
            Self::Intro => "intro",
            Self::MeetingLink => "meeting_link",
            Self::None => "none",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "scheduling" => Self::Scheduling,
            "calendly" => Self::Calendly,
            "share_doc" => Self::ShareDoc,
            "intro" => Self::Intro,
            "meeting_link" | "meetinglink" => Self::MeetingLink,
            _ => Self::None,
        }
    }
    /// Human-friendly label for the "Needs your input" card field.
    pub fn label(self) -> &'static str {
        match self {
            Self::Scheduling => "Proposed meeting time",
            Self::Calendly => "Booking link",
            Self::ShareDoc => "Document link",
            Self::Intro => "Introduction",
            Self::MeetingLink => "Video-call link",
            Self::None => "Detail",
        }
    }
}

/// One detected ask. Mirrors the shadow extractor's JSON contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectedAsk {
    pub text: String,
    #[serde(default)]
    pub resolver_kind: String,
    #[serde(default)]
    pub auto_fillable: bool,
    #[serde(default)]
    pub confidence: Option<f64>,
}

impl DetectedAsk {
    pub fn kind(&self) -> ResolverKind {
        ResolverKind::parse(&self.resolver_kind)
    }
    /// Confidence with a conservative default (treat unscored asks as just
    /// at the floor so an extractor that omits the field still resolves).
    pub fn conf(&self) -> f64 {
        self.confidence.unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AskEnvelope {
    #[serde(default)]
    asks: Vec<DetectedAsk>,
}

/// Mode parsed from `AUGMENTAGENT_ASK_RESOLVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskResolveMode {
    /// Off (default): no extra call, nothing detected.
    Off,
    /// Shadow: extract + log telemetry, never inject / resolve.
    Shadow,
    /// Live: extract, resolve, inject `<resolved_asks>` into the draft.
    Live,
}

impl AskResolveMode {
    /// Read from the environment. Only the exact values `shadow` / `live`
    /// enable the corresponding mode — conservative on purpose (this gates a
    /// per-message Haiku call and external API traffic).
    pub fn from_env() -> Self {
        match std::env::var("AUGMENTAGENT_ASK_RESOLVE").ok().as_deref() {
            Some("shadow") => Self::Shadow,
            Some("live") => Self::Live,
            _ => Self::Off,
        }
    }
    /// Both `shadow` and `live` run the extractor.
    pub fn runs_extractor(self) -> bool {
        matches!(self, Self::Shadow | Self::Live)
    }
}

fn extract_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: ASK_EXTRACT_PROMPT.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
        settings_json: None,
    }
}

fn parse_blob(s: &str) -> Option<AskEnvelope> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<AskEnvelope>(&s[start..=end]).ok()
}

/// Run the extractor over one message body. Returns the detected asks
/// (possibly empty). Never errors to the caller — ask-detect must never
/// affect the real pipeline. Returns an empty vec WITHOUT a model call when
/// the mode does not run the extractor (`Off`).
///
/// Named `_shadow` for back-compat with Phase 1 callers; it now also serves
/// the `live` path (same extractor, the caller decides what to do with the
/// result).
pub async fn detect_asks_shadow<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    mode: AskResolveMode,
    message_body: &str,
) -> Vec<DetectedAsk> {
    if !mode.runs_extractor() || message_body.trim().is_empty() {
        return Vec::new();
    }
    let opts = extract_opts();
    let user = format!("<message>\n{}\n</message>", message_body.trim());
    match reasoner.call(&opts, &user).await {
        Ok(reply) => match parse_blob(&reply) {
            Some(env) => {
                debug!(n = env.asks.len(), "ask-detect: extracted");
                env.asks
            }
            None => {
                warn!("ask-detect: unparseable reply");
                Vec::new()
            }
        },
        Err(e) => {
            warn!("ask-detect call failed: {e:#}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Resolver trait + result type.
// ---------------------------------------------------------------------------

/// What a resolver produces when it can satisfy an ask deterministically.
/// Fed into the drafter as a pre-filled fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFill {
    /// The resolver kind that produced this.
    pub kind: ResolverKind,
    /// Text the drafter can splice in (e.g. a Calendly link, 3 time slots).
    pub fill: String,
}

/// A deterministic ask resolver. `try_resolve` returns `Ok(None)` when this
/// resolver can't (or is disabled by its env flag) fill the ask — that's the
/// safe no-op. `Err` = tried and failed (logged, never fatal).
#[async_trait]
pub trait AskResolver: Send + Sync {
    fn kind(&self) -> ResolverKind;
    async fn try_resolve(&self, ask: &DetectedAsk) -> anyhow::Result<Option<ResolvedFill>>;
}

// ---------------------------------------------------------------------------
// External-service seams. Narrow async traits so resolvers are unit-testable
// with in-memory fakes; the Composio-backed impls live below.
// ---------------------------------------------------------------------------

/// A free/busy interval (UTC) returned by Google Calendar `freebusy.query`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyInterval {
    pub start: chrono::DateTime<Utc>,
    pub end: chrono::DateTime<Utc>,
}

/// Google Calendar free/busy query seam (Composio `GOOGLECALENDAR_FREE_BUSY`).
#[async_trait]
pub trait FreeBusyApi: Send + Sync {
    async fn busy(
        &self,
        entity_id: &str,
        calendar_id: &str,
        time_min: chrono::DateTime<Utc>,
        time_max: chrono::DateTime<Utc>,
    ) -> anyhow::Result<Vec<BusyInterval>>;
}

/// One Drive search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveHit {
    pub name: String,
    pub web_view_link: String,
}

/// Google Drive search seam (Composio `GOOGLEDRIVE_FIND_FILE` /
/// `GOOGLEDRIVE_FIND_FOLDER`-style fulltext query).
#[async_trait]
pub trait DriveSearchApi: Send + Sync {
    async fn search(&self, entity_id: &str, query: &str) -> anyhow::Result<Vec<DriveHit>>;
}

// ---------------------------------------------------------------------------
// Resolver context: everything the live resolvers need, all optional so a
// partially-configured deployment degrades to no-op resolvers rather than
// erroring.
// ---------------------------------------------------------------------------

/// Shared, cheaply-cloneable inputs for the resolver registry.
#[derive(Clone)]
pub struct ResolveCtx {
    /// Composio entity / account id for the calendar + drive owner.
    pub entity_id: Option<String>,
    /// Calendar id for free/busy (defaults to `primary`).
    pub calendar_id: String,
    /// Wiki root for people-page + Calendly/index lookups.
    pub wiki_root: Option<PathBuf>,
    /// Free/busy client (Composio-backed in prod, fake in tests).
    pub freebusy: Option<Arc<dyn FreeBusyApi>>,
    /// Drive search client.
    pub drive: Option<Arc<dyn DriveSearchApi>>,
}

impl Default for ResolveCtx {
    fn default() -> Self {
        Self {
            entity_id: None,
            calendar_id: "primary".into(),
            wiki_root: None,
            freebusy: None,
            drive: None,
        }
    }
}

impl std::fmt::Debug for ResolveCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolveCtx")
            .field("entity_id", &self.entity_id)
            .field("calendar_id", &self.calendar_id)
            .field("wiki_root", &self.wiki_root)
            .field("freebusy", &self.freebusy.is_some())
            .field("drive", &self.drive.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Scheduling resolver: freebusy.query → 3 open business-hours slots.
// ---------------------------------------------------------------------------

/// Reads the next 7 business days of free/busy and proposes up to 3 open
/// 30-min slots inside 09:00–17:00 UTC. Gated by
/// `AUGMENTAGENT_ASK_RESOLVE_SCHEDULING=1`.
pub struct SchedulingResolver {
    ctx: ResolveCtx,
}

impl SchedulingResolver {
    pub fn new(ctx: ResolveCtx) -> Self {
        Self { ctx }
    }
}

const WORK_START_HOUR: u32 = 9;
const WORK_END_HOUR: u32 = 17;
const SLOT_MIN: i64 = 30;
const MAX_SLOTS: usize = 3;
/// Granularity we step the cursor at when scanning for openings. The proposed
/// slot length itself comes from the ask (see [`requested_duration_min`]); we
/// always *advance* on the 30-min grid so longer meetings still land on tidy
/// :00/:30 starts.
const STEP_MIN: i64 = 30;
const DEFAULT_DURATION_MIN: i64 = 30;
const MAX_DURATION_MIN: i64 = 240;

/// Parse a requested meeting length out of the ask text so the scheduler
/// proposes slots that actually fit ("a 45-minute call", "quick 15 min",
/// "1 hour sync", "half an hour"). Falls back to [`DEFAULT_DURATION_MIN`].
/// Deterministic and unit-tested — no model call.
fn requested_duration_min(ask_text: &str) -> i64 {
    let low = ask_text.to_ascii_lowercase();
    if low.contains("half an hour") || low.contains("half-hour") {
        return 30;
    }
    let tokens: Vec<&str> = low
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    for (i, tok) in tokens.iter().enumerate() {
        // "<n> hour(s)" / "<n>h" / "<n> min(ute(s))" / "<n>m".
        let next = tokens.get(i + 1).copied().unwrap_or("");
        if let Ok(n) = tok.parse::<i64>() {
            if next.starts_with("hour") || next.starts_with("hr") {
                return (n * 60).clamp(15, MAX_DURATION_MIN);
            }
            if next.starts_with("min") {
                return n.clamp(5, MAX_DURATION_MIN);
            }
        }
        // Glued forms: "45min", "30m", "1h", "2hr".
        if let Some(num) = tok.strip_suffix("min") {
            if let Ok(n) = num.parse::<i64>() {
                return n.clamp(5, MAX_DURATION_MIN);
            }
        }
        if let Some(num) = tok.strip_suffix("hr") {
            if let Ok(n) = num.parse::<i64>() {
                return (n * 60).clamp(15, MAX_DURATION_MIN);
            }
        }
        if let Some(num) = tok.strip_suffix('h') {
            if let Ok(n) = num.parse::<i64>() {
                return (n * 60).clamp(15, MAX_DURATION_MIN);
            }
        }
        if let Some(num) = tok.strip_suffix('m') {
            if let Ok(n) = num.parse::<i64>() {
                return n.clamp(5, MAX_DURATION_MIN);
            }
        }
    }
    if low.contains("hour") {
        return 60;
    }
    DEFAULT_DURATION_MIN
}

/// Pure slot-finder: given busy intervals, walk the next `days` business days
/// on a 30-min grid inside working hours and return up to `MAX_SLOTS` free
/// starts that fit a `duration_min`-long meeting. Extracted for deterministic
/// testing (no clock/network).
fn first_open_slots(
    now: chrono::DateTime<Utc>,
    busy: &[BusyInterval],
    days: i64,
    duration_min: i64,
) -> Vec<chrono::DateTime<Utc>> {
    let mut out = Vec::new();
    let dur = ChronoDuration::minutes(duration_min.max(SLOT_MIN));
    let step = ChronoDuration::minutes(STEP_MIN);
    let mut cursor = now;
    let horizon = now + ChronoDuration::days(days);
    // Round the cursor up to the next 30-min boundary.
    let minute = cursor.format("%M").to_string().parse::<i64>().unwrap_or(0);
    let bump = (STEP_MIN - (minute % STEP_MIN)) % STEP_MIN;
    cursor += ChronoDuration::minutes(bump);
    cursor = cursor
        - ChronoDuration::seconds(cursor.format("%S").to_string().parse().unwrap_or(0));
    while cursor < horizon && out.len() < MAX_SLOTS {
        let wd = cursor.weekday();
        let hour = cursor.hour_u32();
        let slot_end = cursor + dur;
        let is_business = !matches!(wd, Weekday::Sat | Weekday::Sun);
        // The whole meeting must fit inside working hours — check the END
        // lands at/under WORK_END_HOUR on the same business day.
        let end_ok = slot_end <= cursor
            .date_naive()
            .and_hms_opt(WORK_END_HOUR, 0, 0)
            .map(|nd| chrono::TimeZone::from_utc_datetime(&Utc, &nd))
            .unwrap_or(slot_end);
        let in_hours = hour >= WORK_START_HOUR && end_ok;
        if is_business && in_hours {
            let clashes = busy
                .iter()
                .any(|b| cursor < b.end && slot_end > b.start);
            if !clashes {
                out.push(cursor);
            }
        }
        cursor += step;
    }
    out
}

trait HourU32 {
    fn hour_u32(&self) -> u32;
}
impl HourU32 for chrono::DateTime<Utc> {
    fn hour_u32(&self) -> u32 {
        use chrono::Timelike;
        self.hour()
    }
}

#[async_trait]
impl AskResolver for SchedulingResolver {
    fn kind(&self) -> ResolverKind {
        ResolverKind::Scheduling
    }
    async fn try_resolve(&self, ask: &DetectedAsk) -> anyhow::Result<Option<ResolvedFill>> {
        if !flag_on("AUGMENTAGENT_ASK_RESOLVE_SCHEDULING") {
            return Ok(None);
        }
        if ask.kind() != ResolverKind::Scheduling {
            return Ok(None);
        }
        let (Some(entity), Some(fb)) = (self.ctx.entity_id.as_deref(), self.ctx.freebusy.as_ref())
        else {
            debug!("scheduling resolver: no entity/freebusy client configured");
            return Ok(None);
        };
        let now = Utc::now();
        let horizon = now + ChronoDuration::days(9);
        let busy = fb
            .busy(entity, &self.ctx.calendar_id, now, horizon)
            .await?;
        let duration = requested_duration_min(&ask.text);
        let slots = first_open_slots(now, &busy, 9, duration);
        if slots.is_empty() {
            return Ok(None);
        }
        let rendered = slots
            .iter()
            .map(|s| s.format("%a %b %-d, %H:%M UTC").to_string())
            .collect::<Vec<_>>()
            .join("; ");
        Ok(Some(ResolvedFill {
            kind: ResolverKind::Scheduling,
            fill: format!(
                "Open {duration}-min slots on the user's calendar (UTC): {rendered}. \
                 Offer these concrete times for a {duration}-minute meeting; \
                 do not invent others."
            ),
        }))
    }
}

// ---------------------------------------------------------------------------
// Calendly resolver: config / wiki lookup. No network.
// ---------------------------------------------------------------------------

/// Surfaces the user's stored booking link. Lookup order:
/// 1. `AUGMENTAGENT_CALENDLY_URL` env var.
/// 2. First `https://calendly.com/...` (or `cal.com`) URL found in the wiki
///    `index.md`.
///
/// Gated by `AUGMENTAGENT_ASK_RESOLVE_CALENDLY=1`.
pub struct CalendlyResolver {
    ctx: ResolveCtx,
}

impl CalendlyResolver {
    pub fn new(ctx: ResolveCtx) -> Self {
        Self { ctx }
    }
}

fn find_booking_url(haystack: &str) -> Option<String> {
    for token in haystack.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '<' || c == '>' || c == '"') {
        let t = token.trim_end_matches(['.', ',', ';', ']', ')']);
        let low = t.to_ascii_lowercase();
        if (low.starts_with("https://calendly.com/")
            || low.starts_with("https://cal.com/")
            || low.starts_with("http://calendly.com/"))
            && t.len() > "https://calendly.com/".len()
        {
            return Some(t.to_string());
        }
    }
    None
}

#[async_trait]
impl AskResolver for CalendlyResolver {
    fn kind(&self) -> ResolverKind {
        ResolverKind::Calendly
    }
    async fn try_resolve(&self, ask: &DetectedAsk) -> anyhow::Result<Option<ResolvedFill>> {
        if !flag_on("AUGMENTAGENT_ASK_RESOLVE_CALENDLY") {
            return Ok(None);
        }
        if ask.kind() != ResolverKind::Calendly {
            return Ok(None);
        }
        if let Ok(url) = std::env::var("AUGMENTAGENT_CALENDLY_URL") {
            let url = url.trim();
            if !url.is_empty() {
                return Ok(Some(ResolvedFill {
                    kind: ResolverKind::Calendly,
                    fill: format!("The user's booking link is: {url}"),
                }));
            }
        }
        if let Some(root) = &self.ctx.wiki_root {
            let index = root.join("index.md");
            if let Ok(body) = std::fs::read_to_string(&index) {
                if let Some(url) = find_booking_url(&body) {
                    return Ok(Some(ResolvedFill {
                        kind: ResolverKind::Calendly,
                        fill: format!("The user's booking link is: {url}"),
                    }));
                }
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Meeting-link resolver: stored video-call join link. No network.
// ---------------------------------------------------------------------------

/// Surfaces the user's standing video-call room link (a personal Zoom PMI,
/// Google Meet, or Teams link) when the sender asks for "the meeting link" /
/// "the Zoom". Distinct from Calendly (a booking page) — this is the room you
/// actually join. Lookup order:
/// 1. `AUGMENTAGENT_MEETING_LINK` env var.
/// 2. First Zoom/Meet/Teams URL found in the wiki `index.md`.
///
/// Gated by `AUGMENTAGENT_ASK_RESOLVE_MEETING_LINK=1`.
pub struct MeetingLinkResolver {
    ctx: ResolveCtx,
}

impl MeetingLinkResolver {
    pub fn new(ctx: ResolveCtx) -> Self {
        Self { ctx }
    }
}

/// Recognize a join link for the common video platforms. Trailing markdown /
/// sentence punctuation is stripped (same hygiene as [`find_booking_url`]).
fn find_meeting_url(haystack: &str) -> Option<String> {
    for token in haystack.split(|c: char| {
        c.is_whitespace() || c == '(' || c == ')' || c == '<' || c == '>' || c == '"'
    }) {
        let t = token.trim_end_matches(['.', ',', ';', ']', ')']);
        let low = t.to_ascii_lowercase();
        let is_meet = (low.contains("zoom.us/j/")
            || low.contains("zoom.us/my/")
            || low.starts_with("https://meet.google.com/")
            || low.contains("teams.microsoft.com/l/meetup-join")
            || low.contains("teams.live.com/meet/"))
            && (low.starts_with("http://") || low.starts_with("https://"));
        if is_meet && t.len() > "https://".len() {
            return Some(t.to_string());
        }
    }
    None
}

#[async_trait]
impl AskResolver for MeetingLinkResolver {
    fn kind(&self) -> ResolverKind {
        ResolverKind::MeetingLink
    }
    async fn try_resolve(&self, ask: &DetectedAsk) -> anyhow::Result<Option<ResolvedFill>> {
        if !flag_on("AUGMENTAGENT_ASK_RESOLVE_MEETING_LINK") {
            return Ok(None);
        }
        if ask.kind() != ResolverKind::MeetingLink {
            return Ok(None);
        }
        if let Ok(url) = std::env::var("AUGMENTAGENT_MEETING_LINK") {
            let url = url.trim();
            if !url.is_empty() {
                return Ok(Some(ResolvedFill {
                    kind: ResolverKind::MeetingLink,
                    fill: format!("The user's video-call link is: {url}"),
                }));
            }
        }
        if let Some(root) = &self.ctx.wiki_root {
            let index = root.join("index.md");
            if let Ok(body) = std::fs::read_to_string(&index) {
                if let Some(url) = find_meeting_url(&body) {
                    return Ok(Some(ResolvedFill {
                        kind: ResolverKind::MeetingLink,
                        fill: format!("The user's video-call link is: {url}"),
                    }));
                }
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Share-doc resolver: wiki grep + Drive fulltext search.
// ---------------------------------------------------------------------------

/// Locates a requested document. Tries Drive fulltext search first (most
/// likely to yield a shareable link), then falls back to scanning the wiki
/// for a markdown link whose text matches the doc hint. Gated by
/// `AUGMENTAGENT_ASK_RESOLVE_SHARE_DOC=1`.
pub struct ShareDocResolver {
    ctx: ResolveCtx,
}

impl ShareDocResolver {
    pub fn new(ctx: ResolveCtx) -> Self {
        Self { ctx }
    }
}

/// Pull a short keyword query out of the ask text — strip stopwords, keep the
/// most content-bearing tokens. Deterministic; the extractor already gave us
/// a tight paraphrase so this is mostly cleanup.
fn doc_query(ask_text: &str) -> String {
    const STOP: &[&str] = &[
        "can", "you", "the", "a", "an", "me", "please", "send", "share", "get",
        "could", "would", "i", "to", "of", "for", "with", "your", "my", "do",
        "have", "is", "it", "that", "this", "and", "or", "over", "us",
    ];
    let kept: Vec<&str> = ask_text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| !STOP.contains(&w.to_ascii_lowercase().as_str()))
        .take(6)
        .collect();
    kept.join(" ")
}

#[async_trait]
impl AskResolver for ShareDocResolver {
    fn kind(&self) -> ResolverKind {
        ResolverKind::ShareDoc
    }
    async fn try_resolve(&self, ask: &DetectedAsk) -> anyhow::Result<Option<ResolvedFill>> {
        if !flag_on("AUGMENTAGENT_ASK_RESOLVE_SHARE_DOC") {
            return Ok(None);
        }
        if ask.kind() != ResolverKind::ShareDoc {
            return Ok(None);
        }
        let query = doc_query(&ask.text);
        if query.trim().is_empty() {
            return Ok(None);
        }
        // 1. Drive fulltext.
        if let (Some(entity), Some(drive)) =
            (self.ctx.entity_id.as_deref(), self.ctx.drive.as_ref())
        {
            match drive.search(entity, &query).await {
                Ok(hits) if !hits.is_empty() => {
                    let h = &hits[0];
                    return Ok(Some(ResolvedFill {
                        kind: ResolverKind::ShareDoc,
                        fill: format!(
                            "Matching Drive doc: \"{}\" → {}. Offer this link.",
                            h.name, h.web_view_link
                        ),
                    }));
                }
                Ok(_) => {}
                Err(e) => warn!("share_doc: drive search failed: {e:#}"),
            }
        }
        // 2. Wiki markdown-link fallback.
        if let Some(root) = &self.ctx.wiki_root {
            if let Some(link) = grep_wiki_link(root, &query) {
                return Ok(Some(ResolvedFill {
                    kind: ResolverKind::ShareDoc,
                    fill: format!("Likely doc from the wiki: {link}. Offer this link."),
                }));
            }
        }
        Ok(None)
    }
}

/// Shallow scan of `index.md` + `projects/*.md` for a `[text](url)` link whose
/// text contains any query word (case-insensitive). Bounded — wiki is small.
fn grep_wiki_link(root: &std::path::Path, query: &str) -> Option<String> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    if words.is_empty() {
        return None;
    }
    let mut files: Vec<PathBuf> = vec![root.join("index.md")];
    if let Ok(rd) = std::fs::read_dir(root.join("projects")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("md") {
                files.push(p);
            }
        }
    }
    for f in files {
        let Ok(body) = std::fs::read_to_string(&f) else {
            continue;
        };
        for (text, url) in markdown_links(&body) {
            let low = text.to_ascii_lowercase();
            if words.iter().any(|w| low.contains(w.as_str()))
                && (url.starts_with("http://") || url.starts_with("https://"))
            {
                return Some(url);
            }
        }
    }
    None
}

/// Minimal `[text](url)` extractor — good enough for the wiki's controlled
/// markdown; not a general parser.
fn markdown_links(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close) = body[i + 1..].find(']') {
                let text_end = i + 1 + close;
                if text_end + 1 < bytes.len() && bytes[text_end + 1] == b'(' {
                    if let Some(pclose) = body[text_end + 2..].find(')') {
                        let url_end = text_end + 2 + pclose;
                        out.push((
                            body[i + 1..text_end].to_string(),
                            body[text_end + 2..url_end].to_string(),
                        ));
                        i = url_end + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Intro resolver: wiki people-page lookup. SUGGESTION ONLY — never executes.
// ---------------------------------------------------------------------------

/// Looks up the intro target in the wiki identity index. Returns a *suggestion*
/// fill that explicitly tells the drafter the intro is gated on the user's
/// explicit OK — it never produces a "send the intro" instruction. Gated by
/// `AUGMENTAGENT_ASK_RESOLVE_INTRO=1`.
pub struct IntroResolver {
    ctx: ResolveCtx,
}

impl IntroResolver {
    pub fn new(ctx: ResolveCtx) -> Self {
        Self { ctx }
    }
}

/// Pull a candidate person name out of the ask text. The extractor's
/// paraphrase usually contains it; we take the longest run of Capitalized
/// words as the name heuristic.
fn guess_target_name(ask_text: &str) -> Option<String> {
    let mut best: Vec<&str> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for tok in ask_text.split_whitespace() {
        let clean = tok.trim_matches(|c: char| !c.is_alphanumeric());
        let is_cap = clean
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
            && clean.len() > 1;
        if is_cap {
            cur.push(clean);
        } else {
            if cur.len() > best.len() {
                best = cur.clone();
            }
            cur.clear();
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    if best.is_empty() {
        None
    } else {
        Some(best.join(" "))
    }
}

#[async_trait]
impl AskResolver for IntroResolver {
    fn kind(&self) -> ResolverKind {
        ResolverKind::Intro
    }
    async fn try_resolve(&self, ask: &DetectedAsk) -> anyhow::Result<Option<ResolvedFill>> {
        if !flag_on("AUGMENTAGENT_ASK_RESOLVE_INTRO") {
            return Ok(None);
        }
        if ask.kind() != ResolverKind::Intro {
            return Ok(None);
        }
        let Some(root) = &self.ctx.wiki_root else {
            return Ok(None);
        };
        let Some(name) = guess_target_name(&ask.text) else {
            return Ok(None);
        };
        let layout = augmentagent_wiki::WikiLayout::new(root.clone());
        let index = match augmentagent_wiki::IdentityIndex::build(&layout) {
            Ok(i) => i,
            Err(e) => {
                warn!("intro resolver: identity index build failed: {e:#}");
                return Ok(None);
            }
        };
        // Match a people page whose slug contains the lowercased name tokens.
        let needle: Vec<String> = name
            .split_whitespace()
            .map(|w| w.to_ascii_lowercase())
            .collect();
        let hit = index.pages().iter().find(|p| {
            let slug = p.slug.to_ascii_lowercase();
            needle.iter().all(|w| slug.contains(w.as_str()))
        });
        let Some(page) = hit else {
            return Ok(None);
        };
        let email = page.identities.email.first().cloned();
        // NOTE: suggestion only. The drafter is told the user must explicitly
        // approve before any intro is actually sent — intros are reputational
        // and never auto-execute (issue #35 open question, settled).
        let who = match email {
            Some(e) => format!("{name} (on file: {e})"),
            None => format!("{name} (in the wiki, no email on file)"),
        };
        Ok(Some(ResolvedFill {
            kind: ResolverKind::Intro,
            fill: format!(
                "SUGGESTION (do NOT commit to or send an intro): {who} appears to be \
                 the intro target and is known to the user. You may acknowledge the \
                 request and say the user will consider it — but you MUST NOT promise \
                 or perform the introduction. Intros require the user's explicit \
                 separate approval."
            ),
        }))
    }
}

// ---------------------------------------------------------------------------
// Registry + orchestration.
// ---------------------------------------------------------------------------

/// Build the live resolver registry from a context. All four are always
/// constructed; each self-gates on its own env flag inside `try_resolve`, so
/// an unconfigured/flag-off resolver is a guaranteed no-op.
pub fn live_resolvers(ctx: ResolveCtx) -> Vec<Arc<dyn AskResolver>> {
    vec![
        Arc::new(SchedulingResolver::new(ctx.clone())),
        Arc::new(CalendlyResolver::new(ctx.clone())),
        Arc::new(MeetingLinkResolver::new(ctx.clone())),
        Arc::new(ShareDocResolver::new(ctx.clone())),
        Arc::new(IntroResolver::new(ctx)),
    ]
}

/// Back-compat alias for Phase 1 callers/tests. Equivalent to
/// `live_resolvers(ResolveCtx::default())` — every resolver self-gates so
/// these are all no-ops without env flags + a configured context.
pub fn default_resolvers() -> Vec<Arc<dyn AskResolver>> {
    live_resolvers(ResolveCtx::default())
}

/// Run one ask against the resolver whose `kind()` matches it. Errors are
/// logged and swallowed (returns `None`) — a resolver failure must never
/// break drafting.
async fn resolve_one(
    resolvers: &[Arc<dyn AskResolver>],
    ask: &DetectedAsk,
) -> Option<ResolvedFill> {
    let kind = ask.kind();
    if kind == ResolverKind::None {
        return None;
    }
    for r in resolvers {
        if r.kind() == kind {
            match r.try_resolve(ask).await {
                Ok(Some(f)) => return Some(f),
                Ok(None) => return None,
                Err(e) => {
                    warn!(kind = kind.as_str(), "resolver failed: {e:#}");
                    return None;
                }
            }
        }
    }
    None
}

/// Render the `<resolved_asks>` block from successful fills. Empty input ⇒
/// empty string (caller treats that as "inject nothing", byte-identical
/// draft prompt).
pub fn resolved_asks_block(fills: &[ResolvedFill]) -> String {
    if fills.is_empty() {
        return String::new();
    }
    let mut s = String::from("<resolved_asks>\n");
    for f in fills {
        s.push_str("- [");
        s.push_str(f.kind.as_str());
        s.push_str("] ");
        s.push_str(f.fill.trim());
        s.push('\n');
    }
    s.push_str("</resolved_asks>");
    s
}

/// One detected ask that cleared the confidence floor and maps to a real
/// resolver kind, but which the matching resolver could NOT fill (returned
/// `Ok(None)` — no calendar configured, doc not found, person not in the
/// wiki, …). These are surfaced to the human on the Discord approval card as
/// a "Needs your input" field so the value can be supplied inline instead of
/// the drafter inventing a placeholder.
///
/// This is *only* ever produced in `AUGMENTAGENT_ASK_RESOLVE=live`; in
/// off/shadow the resolver stage never runs, so [`ResolveOutcome::unresolved`]
/// is always empty and the draft + card stay byte-identical to today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedAsk {
    /// The resolver kind that *would* have handled it (scheduling, etc.).
    pub kind: ResolverKind,
    /// The extractor's tight paraphrase of what was asked — shown verbatim on
    /// the card so the user knows exactly what to supply.
    pub text: String,
}

/// Result of the full resolve stage: the `<resolved_asks>` prompt block (fed
/// to the drafter, exactly as before) plus the list of asks that need human
/// input. Both are empty in any non-`live` mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveOutcome {
    /// `<resolved_asks>` fragment for `draft_user_message`, or empty.
    pub block: String,
    /// Asks that cleared the floor but no resolver could fill.
    pub unresolved: Vec<UnresolvedAsk>,
}

/// End-to-end Phase-2/3 entry point: extract asks, resolve the
/// high-confidence ones in parallel under a wall-clock budget, and return
/// both the composed `<resolved_asks>` block AND the list of asks no resolver
/// could fill (for the "Needs your input" card field, #35 Phase 5).
///
/// Both halves are empty — meaning "inject nothing / nothing to ask the user,
/// today's behavior" — when: mode is not `Live`, no asks clear the confidence
/// floor, or the budget is exceeded. The whole stage is best-effort: a
/// timeout or error degrades to an empty outcome, never an error to the
/// caller.
pub async fn resolve_asks<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    mode: AskResolveMode,
    message_body: &str,
    ctx: ResolveCtx,
) -> ResolveOutcome {
    if mode != AskResolveMode::Live {
        return ResolveOutcome::default();
    }
    let asks = detect_asks_shadow(reasoner, mode, message_body).await;
    if asks.is_empty() {
        return ResolveOutcome::default();
    }
    let floor = confidence_floor();
    let eligible: Vec<DetectedAsk> = asks
        .into_iter()
        .filter(|a| a.kind() != ResolverKind::None && a.conf() >= floor)
        .collect();
    if eligible.is_empty() {
        debug!("ask-resolve: no asks cleared the confidence floor");
        return ResolveOutcome::default();
    }
    let resolvers = live_resolvers(ctx);
    let fills = match tokio::time::timeout(
        RESOLVE_BUDGET,
        resolve_all(&resolvers, &eligible),
    )
    .await
    {
        Ok(v) => v,
        Err(_) => {
            warn!("ask-resolve: budget exceeded, drafting without resolved asks");
            return ResolveOutcome::default();
        }
    };
    // An eligible ask is "unresolved" when no successful fill shares its
    // resolver kind. De-dupe by kind so the card lists each missing thing
    // once (mirrors the de-dupe in `resolve_all`).
    let mut unresolved: Vec<UnresolvedAsk> = Vec::new();
    for a in &eligible {
        let kind = a.kind();
        let filled = fills.iter().any(|f| f.kind == kind);
        let already = unresolved.iter().any(|u| u.kind == kind);
        if !filled && !already {
            unresolved.push(UnresolvedAsk {
                kind,
                text: a.text.trim().to_string(),
            });
        }
    }
    if !fills.is_empty() {
        info!(n = fills.len(), "ask-resolve: injecting <resolved_asks>");
    }
    if !unresolved.is_empty() {
        info!(
            n = unresolved.len(),
            "ask-resolve: asks need human input (surfacing on card)"
        );
    }
    ResolveOutcome {
        block: resolved_asks_block(&fills),
        unresolved,
    }
}

/// Back-compat thin wrapper: the `<resolved_asks>` block only. Callers that
/// don't render the "Needs your input" card field (every non-email channel)
/// keep using this and stay byte-identical. Equivalent to
/// `resolve_asks(...).block`.
///
/// Returns `String::new()` — meaning "inject nothing, today's behavior" —
/// under exactly the same conditions as [`resolve_asks`].
pub async fn resolve_asks_block<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    mode: AskResolveMode,
    message_body: &str,
    ctx: ResolveCtx,
) -> String {
    resolve_asks(reasoner, mode, message_body, ctx).await.block
}

/// Resolve every eligible ask concurrently. Uses `tokio::join!` for the
/// common small fan-out (≤4) and falls back to `join_all` for larger sets.
async fn resolve_all(
    resolvers: &[Arc<dyn AskResolver>],
    asks: &[DetectedAsk],
) -> Vec<ResolvedFill> {
    // De-dupe by kind: at most one fill per resolver kind keeps the block
    // tight and avoids redundant external calls when the extractor emits two
    // near-identical asks.
    let mut by_kind: Vec<&DetectedAsk> = Vec::new();
    for a in asks {
        if !by_kind.iter().any(|x| x.kind() == a.kind()) {
            by_kind.push(a);
        }
    }
    match by_kind.as_slice() {
        [] => Vec::new(),
        [a] => resolve_one(resolvers, a).await.into_iter().collect(),
        [a, b] => {
            let (ra, rb) =
                tokio::join!(resolve_one(resolvers, a), resolve_one(resolvers, b));
            [ra, rb].into_iter().flatten().collect()
        }
        [a, b, c] => {
            let (ra, rb, rc) = tokio::join!(
                resolve_one(resolvers, a),
                resolve_one(resolvers, b),
                resolve_one(resolvers, c),
            );
            [ra, rb, rc].into_iter().flatten().collect()
        }
        [a, b, c, d] => {
            let (ra, rb, rc, rd) = tokio::join!(
                resolve_one(resolvers, a),
                resolve_one(resolvers, b),
                resolve_one(resolvers, c),
                resolve_one(resolvers, d),
            );
            [ra, rb, rc, rd].into_iter().flatten().collect()
        }
        many => {
            // ≥5 distinct resolver kinds (all 5 today): spawn each onto the
            // runtime and join — still concurrent, still under RESOLVE_BUDGET.
            let mut handles = Vec::with_capacity(many.len());
            for a in many {
                let rs = resolvers.to_vec();
                let ask = (*a).clone();
                handles.push(tokio::spawn(async move {
                    resolve_one(&rs, &ask).await
                }));
            }
            let mut out = Vec::new();
            for h in handles {
                if let Ok(Some(f)) = h.await {
                    out.push(f);
                }
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Composio-backed client impls. Self-contained reqwest clients mirroring the
// INTENTIONAL duplication endorsed in
// `crates/augmentagent-channel-gdrive/src/composio.rs` (extracting a shared
// crate would force edits to the prod email path).
// ---------------------------------------------------------------------------

/// Composio v3 client used by the free/busy + drive seams. 3 attempts,
/// 429/5xx/transient retried with exponential backoff (same policy as the
/// calendar/gdrive crates).
pub struct ComposioResolveClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ComposioResolveClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: "https://backend.composio.dev".into(),
            api_key,
        }
    }
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    async fn execute(
        &self,
        action: &str,
        entity_id: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}/api/v3/tools/execute/{}", self.base_url, action);
        let body = serde_json::json!({ "user_id": entity_id, "arguments": arguments });
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp.json::<serde_json::Value>().await?);
                    }
                    let retryable =
                        status.as_u16() == 429 || status.is_server_error();
                    let text = resp.text().await.unwrap_or_default();
                    if retryable && attempt < MAX_ATTEMPTS {
                        warn!(action, %status, attempt, "composio retryable; backing off");
                        backoff(attempt).await;
                        continue;
                    }
                    anyhow::bail!("{action} → {status}: {text}");
                }
                Err(e)
                    if attempt < MAX_ATTEMPTS
                        && (e.is_timeout() || e.is_connect() || e.is_request()) =>
                {
                    warn!(action, attempt, "composio transport error; retrying: {e}");
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

async fn backoff(attempt: u32) {
    let base_ms: u64 = 300;
    let mult: u64 = 1 << attempt.min(5);
    tokio::time::sleep(Duration::from_millis(base_ms * mult)).await;
}

/// Recursively find the first array under any of `keys` (tolerates Composio's
/// variable `data` / `data.response_data` nesting).
fn find_array<'a>(
    v: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a Vec<serde_json::Value>> {
    match v {
        serde_json::Value::Object(m) => {
            for k in keys {
                if let Some(serde_json::Value::Array(a)) = m.get(*k) {
                    return Some(a);
                }
            }
            m.values().find_map(|x| find_array(x, keys))
        }
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_array(x, keys)),
        _ => None,
    }
}

#[async_trait]
impl FreeBusyApi for ComposioResolveClient {
    async fn busy(
        &self,
        entity_id: &str,
        calendar_id: &str,
        time_min: chrono::DateTime<Utc>,
        time_max: chrono::DateTime<Utc>,
    ) -> anyhow::Result<Vec<BusyInterval>> {
        let args = serde_json::json!({
            "timeMin": time_min.to_rfc3339(),
            "timeMax": time_max.to_rfc3339(),
            "items": [{ "id": calendar_id }],
        });
        let v = self
            .execute("GOOGLECALENDAR_FREE_BUSY", entity_id, args)
            .await?;
        // Response shape: calendars.<id>.busy = [{start,end}]. Composio may
        // nest under data/response_data — search recursively for any `busy`
        // array of {start,end}.
        let mut out = Vec::new();
        if let Some(arr) = find_array(&v, &["busy"]) {
            for it in arr {
                let s = it.get("start").and_then(|x| x.as_str());
                let e = it.get("end").and_then(|x| x.as_str());
                if let (Some(s), Some(e)) = (s, e) {
                    if let (Ok(s), Ok(e)) = (
                        chrono::DateTime::parse_from_rfc3339(s),
                        chrono::DateTime::parse_from_rfc3339(e),
                    ) {
                        out.push(BusyInterval {
                            start: s.with_timezone(&Utc),
                            end: e.with_timezone(&Utc),
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl DriveSearchApi for ComposioResolveClient {
    async fn search(
        &self,
        entity_id: &str,
        query: &str,
    ) -> anyhow::Result<Vec<DriveHit>> {
        // Composio's GOOGLEDRIVE_FIND_FILE takes a `query` (Drive `q` syntax
        // or fulltext). We pass a fulltext `name contains` style query.
        let args = serde_json::json!({
            "query": format!("fullText contains '{}'", query.replace('\'', " ")),
        });
        let v = self
            .execute("GOOGLEDRIVE_FIND_FILE", entity_id, args)
            .await?;
        let mut out = Vec::new();
        if let Some(arr) = find_array(&v, &["files", "data"]) {
            for f in arr {
                let name = f
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                let link = f
                    .get("webViewLink")
                    .or_else(|| f.get("web_view_link"))
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !name.is_empty() && !link.is_empty() {
                    out.push(DriveHit {
                        name,
                        web_view_link: link,
                    });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Env vars are process-global; cargo runs tests in parallel threads. Any
    /// test that mutates `AUGMENTAGENT_*` must hold this lock for its whole
    /// body so reads/writes don't race across tests. Poison-tolerant: a
    /// panicking test must not wedge the rest of the suite.
    fn env_lock() -> MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        match L.get_or_init(|| Mutex::new(())).lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    struct CannedReasoner(Mutex<Option<String>>);
    #[async_trait]
    impl Reasoner for CannedReasoner {
        async fn call(&self, _o: &ReasonerOpts, _u: &str) -> anyhow::Result<String> {
            match self.0.lock().unwrap().take() {
                Some(s) => Ok(s),
                None => anyhow::bail!("no canned reply"),
            }
        }
    }
    fn reasoner(reply: Option<&str>) -> Arc<CannedReasoner> {
        Arc::new(CannedReasoner(Mutex::new(reply.map(String::from))))
    }

    struct EnvGuard(&'static str);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }
    fn set(k: &'static str, v: &str) -> EnvGuard {
        std::env::set_var(k, v);
        EnvGuard(k)
    }

    fn ask(kind: &str, conf: f64) -> DetectedAsk {
        DetectedAsk {
            text: "the ask".into(),
            resolver_kind: kind.into(),
            auto_fillable: true,
            confidence: Some(conf),
        }
    }

    #[test]
    fn mode_from_env_off_shadow_live() {
        let _env = env_lock();
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE");
        assert_eq!(AskResolveMode::from_env(), AskResolveMode::Off);
        let _g = set("AUGMENTAGENT_ASK_RESOLVE", "shadow");
        assert_eq!(AskResolveMode::from_env(), AskResolveMode::Shadow);
        let _g = set("AUGMENTAGENT_ASK_RESOLVE", "live");
        assert_eq!(AskResolveMode::from_env(), AskResolveMode::Live);
        assert!(AskResolveMode::Live.runs_extractor());
        assert!(AskResolveMode::Shadow.runs_extractor());
        assert!(!AskResolveMode::Off.runs_extractor());
        let _g = set("AUGMENTAGENT_ASK_RESOLVE", "on");
        assert_eq!(AskResolveMode::from_env(), AskResolveMode::Off);
    }

    #[tokio::test]
    async fn off_mode_makes_no_call_and_returns_empty() {
        let r = reasoner(None);
        let asks = detect_asks_shadow(&r, AskResolveMode::Off, "can we meet tuesday?").await;
        assert!(asks.is_empty());
    }

    #[tokio::test]
    async fn shadow_parses_asks() {
        let r = reasoner(Some(
            r#"{"asks":[{"text":"can we meet next week","resolver_kind":"scheduling","auto_fillable":true,"confidence":0.8}]}"#,
        ));
        let asks =
            detect_asks_shadow(&r, AskResolveMode::Shadow, "Hey, can we meet next week?").await;
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].kind(), ResolverKind::Scheduling);
        assert!(asks[0].auto_fillable);
    }

    #[tokio::test]
    async fn shadow_unparseable_is_empty_not_fatal() {
        let r = reasoner(Some("not json"));
        let asks = detect_asks_shadow(&r, AskResolveMode::Shadow, "hi").await;
        assert!(asks.is_empty());
    }

    #[tokio::test]
    async fn resolvers_noop_without_flags() {
        let _env = env_lock();
        // No per-resolver env flags ⇒ every live resolver is a guaranteed
        // no-op even with a fully-populated context.
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE_SCHEDULING");
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE_CALENDLY");
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE_MEETING_LINK");
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE_SHARE_DOC");
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE_INTRO");
        let ctx = ResolveCtx::default();
        for r in live_resolvers(ctx) {
            assert!(r
                .try_resolve(&ask(r.kind().as_str(), 0.9))
                .await
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn first_open_slots_skips_busy_and_weekends() {
        // Friday 2026-05-15 08:00 UTC. Slots should land Fri >=09:00 then,
        // skipping the weekend, Monday.
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 8, 0, 0).unwrap();
        let busy = vec![BusyInterval {
            start: Utc.with_ymd_and_hms(2026, 5, 15, 9, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 5, 15, 16, 0, 0).unwrap(),
        }];
        let slots = first_open_slots(now, &busy, 9, 30);
        assert_eq!(slots.len(), 3);
        // First free slot is Fri 16:00 (after the busy block, before 17:00).
        assert_eq!(slots[0], Utc.with_ymd_and_hms(2026, 5, 15, 16, 0, 0).unwrap());
        // No slot may fall on Sat/Sun.
        for s in &slots {
            assert!(!matches!(s.weekday(), Weekday::Sat | Weekday::Sun));
        }
    }

    #[test]
    fn first_open_slots_all_busy_returns_empty() {
        let now = Utc.with_ymd_and_hms(2026, 5, 18, 8, 0, 0).unwrap(); // Monday
        let busy = vec![BusyInterval {
            start: Utc.with_ymd_and_hms(2026, 5, 18, 0, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 5, 30, 0, 0, 0).unwrap(),
        }];
        assert!(first_open_slots(now, &busy, 9, 30).is_empty());
    }

    struct FakeFb(Vec<BusyInterval>);
    #[async_trait]
    impl FreeBusyApi for FakeFb {
        async fn busy(
            &self,
            _e: &str,
            _c: &str,
            _a: chrono::DateTime<Utc>,
            _b: chrono::DateTime<Utc>,
        ) -> anyhow::Result<Vec<BusyInterval>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn scheduling_resolver_live_path() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_SCHEDULING", "1");
        let ctx = ResolveCtx {
            entity_id: Some("ent".into()),
            freebusy: Some(Arc::new(FakeFb(vec![]))),
            ..Default::default()
        };
        let r = SchedulingResolver::new(ctx);
        let out = r.try_resolve(&ask("scheduling", 0.9)).await.unwrap();
        let f = out.expect("should resolve with empty calendar");
        assert_eq!(f.kind, ResolverKind::Scheduling);
        assert!(f.fill.contains("UTC"));
    }

    #[tokio::test]
    async fn scheduling_resolver_wrong_kind_is_none() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_SCHEDULING", "1");
        let ctx = ResolveCtx {
            entity_id: Some("ent".into()),
            freebusy: Some(Arc::new(FakeFb(vec![]))),
            ..Default::default()
        };
        let r = SchedulingResolver::new(ctx);
        assert!(r.try_resolve(&ask("intro", 0.9)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn calendly_resolver_env_then_wiki() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_CALENDLY", "1");
        let _u = set("AUGMENTAGENT_CALENDLY_URL", "https://calendly.com/nolan/30min");
        let r = CalendlyResolver::new(ResolveCtx::default());
        let f = r.try_resolve(&ask("calendly", 0.9)).await.unwrap().unwrap();
        assert!(f.fill.contains("calendly.com/nolan/30min"));
    }

    #[tokio::test]
    async fn calendly_resolver_from_wiki_index() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_CALENDLY", "1");
        std::env::remove_var("AUGMENTAGENT_CALENDLY_URL");
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("index.md"),
            "# Wiki\n\nBook me: https://calendly.com/team/intro-chat (15 min)\n",
        )
        .unwrap();
        let ctx = ResolveCtx {
            wiki_root: Some(d.path().to_path_buf()),
            ..Default::default()
        };
        let r = CalendlyResolver::new(ctx);
        let f = r.try_resolve(&ask("calendly", 0.9)).await.unwrap().unwrap();
        assert!(f.fill.contains("calendly.com/team/intro-chat"));
    }

    struct FakeDrive(Vec<DriveHit>);
    #[async_trait]
    impl DriveSearchApi for FakeDrive {
        async fn search(&self, _e: &str, _q: &str) -> anyhow::Result<Vec<DriveHit>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn share_doc_drive_hit() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_SHARE_DOC", "1");
        let ctx = ResolveCtx {
            entity_id: Some("ent".into()),
            drive: Some(Arc::new(FakeDrive(vec![DriveHit {
                name: "Q4 Investor Deck".into(),
                web_view_link: "https://drive.google.com/file/Q4".into(),
            }]))),
            ..Default::default()
        };
        let r = ShareDocResolver::new(ctx);
        let mut a = ask("share_doc", 0.9);
        a.text = "can you send me the Q4 investor deck".into();
        let f = r.try_resolve(&a).await.unwrap().unwrap();
        assert!(f.fill.contains("drive.google.com/file/Q4"));
    }

    #[tokio::test]
    async fn share_doc_wiki_link_fallback() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_SHARE_DOC", "1");
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("index.md"),
            "# Wiki\n\nThe [Q4 deck](https://docs.example.com/q4) is here.\n",
        )
        .unwrap();
        let ctx = ResolveCtx {
            wiki_root: Some(d.path().to_path_buf()),
            ..Default::default()
        };
        let r = ShareDocResolver::new(ctx);
        let mut a = ask("share_doc", 0.9);
        a.text = "share the Q4 deck".into();
        let f = r.try_resolve(&a).await.unwrap().unwrap();
        assert!(f.fill.contains("docs.example.com/q4"));
    }

    #[tokio::test]
    async fn intro_resolver_is_suggestion_only_and_never_executes() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_INTRO", "1");
        let d = tempfile::tempdir().unwrap();
        let people = d.path().join("people");
        std::fs::create_dir_all(&people).unwrap();
        std::fs::write(
            people.join("sarah_chen_at_acme_com.md"),
            "---\nkind: person\nkey: sarah\nidentities:\n  email: [sarah.chen@acme.com]\n---\n\n# Sarah Chen\n",
        )
        .unwrap();
        let ctx = ResolveCtx {
            wiki_root: Some(d.path().to_path_buf()),
            ..Default::default()
        };
        let r = IntroResolver::new(ctx);
        let mut a = ask("intro", 0.9);
        a.text = "can you intro me to Sarah Chen who runs AI infra".into();
        let f = r.try_resolve(&a).await.unwrap().unwrap();
        assert_eq!(f.kind, ResolverKind::Intro);
        // Hard guarantee: never an execution instruction.
        assert!(f.fill.contains("SUGGESTION"));
        assert!(f.fill.contains("MUST NOT"));
        assert!(f.fill.contains("explicit"));
        assert!(f.fill.contains("sarah.chen@acme.com"));
    }

    #[tokio::test]
    async fn intro_resolver_unknown_person_is_none() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_INTRO", "1");
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("people")).unwrap();
        let ctx = ResolveCtx {
            wiki_root: Some(d.path().to_path_buf()),
            ..Default::default()
        };
        let r = IntroResolver::new(ctx);
        let mut a = ask("intro", 0.9);
        a.text = "intro me to Nobody Here".into();
        assert!(r.try_resolve(&a).await.unwrap().is_none());
    }

    #[test]
    fn resolved_asks_block_empty_is_empty_string() {
        assert_eq!(resolved_asks_block(&[]), "");
    }

    #[test]
    fn resolved_asks_block_renders_kinds() {
        let b = resolved_asks_block(&[
            ResolvedFill {
                kind: ResolverKind::Calendly,
                fill: "link: x".into(),
            },
            ResolvedFill {
                kind: ResolverKind::Scheduling,
                fill: "slots: y".into(),
            },
        ]);
        assert!(b.starts_with("<resolved_asks>\n"));
        assert!(b.trim_end().ends_with("</resolved_asks>"));
        assert!(b.contains("[calendly] link: x"));
        assert!(b.contains("[scheduling] slots: y"));
    }

    #[tokio::test]
    async fn resolve_asks_block_off_and_shadow_inject_nothing() {
        let _env = env_lock();
        let r = reasoner(Some(
            r#"{"asks":[{"text":"x","resolver_kind":"calendly","confidence":0.9}]}"#,
        ));
        // Off: empty regardless.
        assert_eq!(
            resolve_asks_block(&r, AskResolveMode::Off, "book me", ResolveCtx::default()).await,
            ""
        );
        // Shadow: extractor may run but injection is suppressed.
        let r2 = reasoner(Some(
            r#"{"asks":[{"text":"x","resolver_kind":"calendly","confidence":0.9}]}"#,
        ));
        assert_eq!(
            resolve_asks_block(&r2, AskResolveMode::Shadow, "book me", ResolveCtx::default())
                .await,
            ""
        );
    }

    #[tokio::test]
    async fn resolve_asks_block_below_floor_injects_nothing() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_CALENDLY", "1");
        let _u = set("AUGMENTAGENT_CALENDLY_URL", "https://calendly.com/x/y");
        let r = reasoner(Some(
            r#"{"asks":[{"text":"got a calendly?","resolver_kind":"calendly","confidence":0.4}]}"#,
        ));
        // 0.4 < 0.7 floor ⇒ nothing.
        assert_eq!(
            resolve_asks_block(&r, AskResolveMode::Live, "got a calendly?", ResolveCtx::default())
                .await,
            ""
        );
    }

    #[tokio::test]
    async fn resolve_asks_block_live_end_to_end() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_CALENDLY", "1");
        let _u = set("AUGMENTAGENT_CALENDLY_URL", "https://calendly.com/nolan/30");
        let r = reasoner(Some(
            r#"{"asks":[{"text":"got a calendly?","resolver_kind":"calendly","confidence":0.92}]}"#,
        ));
        let block = resolve_asks_block(
            &r,
            AskResolveMode::Live,
            "Hey, got a Calendly link?",
            ResolveCtx::default(),
        )
        .await;
        assert!(block.contains("<resolved_asks>"));
        assert!(block.contains("calendly.com/nolan/30"));
        assert!(block.contains("[calendly]"));
    }

    #[test]
    fn resolver_kind_roundtrips() {
        for k in [
            ResolverKind::Scheduling,
            ResolverKind::Calendly,
            ResolverKind::ShareDoc,
            ResolverKind::Intro,
            ResolverKind::MeetingLink,
            ResolverKind::None,
        ] {
            assert_eq!(ResolverKind::parse(k.as_str()), k);
        }
        assert_eq!(ResolverKind::parse("meetinglink"), ResolverKind::MeetingLink);
        assert_eq!(ResolverKind::parse("weird"), ResolverKind::None);
    }

    #[test]
    fn doc_query_strips_stopwords() {
        assert_eq!(doc_query("can you send me the Q4 investor deck"), "Q4 investor deck");
    }

    #[test]
    fn guess_target_name_picks_capitalized_run() {
        assert_eq!(
            guess_target_name("can you intro me to Sarah Chen about infra").as_deref(),
            Some("Sarah Chen")
        );
    }

    #[test]
    fn find_booking_url_handles_markdown_and_trailing_punct() {
        assert_eq!(
            find_booking_url("book here (https://calendly.com/a/b)."),
            Some("https://calendly.com/a/b".to_string())
        );
        assert_eq!(find_booking_url("no link here"), None);
    }

    // --- #35 Phase 3/5: duration parsing, multi-slot, meeting-link
    // resolver, and the "needs your input" (`resolve_asks`) path. ---

    #[test]
    fn requested_duration_parses_common_forms() {
        assert_eq!(requested_duration_min("got 30 min to chat?"), 30);
        assert_eq!(requested_duration_min("a quick 15 minute call"), 15);
        assert_eq!(requested_duration_min("can we do a 45-minute sync"), 45);
        assert_eq!(requested_duration_min("let's grab an hour"), 60);
        assert_eq!(requested_duration_min("1 hour deep dive"), 60);
        assert_eq!(requested_duration_min("2 hours workshop"), 120);
        assert_eq!(requested_duration_min("30m standup"), 30);
        assert_eq!(requested_duration_min("a 90min review"), 90);
        assert_eq!(requested_duration_min("2hr planning"), 120);
        assert_eq!(requested_duration_min("half an hour works"), 30);
        // No duration hint ⇒ default.
        assert_eq!(
            requested_duration_min("can we meet next week"),
            DEFAULT_DURATION_MIN
        );
        // Absurd values are clamped, never propagate.
        assert!(requested_duration_min("a 999 hour call") <= MAX_DURATION_MIN);
    }

    #[test]
    fn first_open_slots_respects_requested_duration() {
        // Friday 2026-05-15 14:00. A 90-min meeting starting 16:00 would end
        // 17:30 — past WORK_END_HOUR — so 16:00 must be rejected for 90min
        // but accepted for 30min.
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 13, 0, 0).unwrap();
        let busy = vec![BusyInterval {
            start: Utc.with_ymd_and_hms(2026, 5, 15, 13, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 5, 15, 16, 0, 0).unwrap(),
        }];
        let short = first_open_slots(now, &busy, 9, 30);
        assert!(short.contains(&Utc.with_ymd_and_hms(2026, 5, 15, 16, 0, 0).unwrap()));
        let long = first_open_slots(now, &busy, 9, 90);
        // 16:00 + 90min = 17:30 > 17:00 ⇒ not offered Friday; rolls to Monday.
        assert!(!long
            .iter()
            .any(|s| *s == Utc.with_ymd_and_hms(2026, 5, 15, 16, 0, 0).unwrap()));
        for s in &long {
            assert!(!matches!(s.weekday(), Weekday::Sat | Weekday::Sun));
        }
    }

    #[tokio::test]
    async fn scheduling_resolver_uses_ask_duration_in_fill() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_SCHEDULING", "1");
        let ctx = ResolveCtx {
            entity_id: Some("ent".into()),
            freebusy: Some(Arc::new(FakeFb(vec![]))),
            ..Default::default()
        };
        let r = SchedulingResolver::new(ctx);
        let mut a = ask("scheduling", 0.9);
        a.text = "can we book a 45 minute call next week".into();
        let f = r.try_resolve(&a).await.unwrap().unwrap();
        assert!(f.fill.contains("45-min"));
        assert!(f.fill.contains("45-minute meeting"));
    }

    #[test]
    fn find_meeting_url_recognizes_platforms() {
        assert_eq!(
            find_meeting_url("join: https://us05web.zoom.us/j/123456789?pwd=ab"),
            Some("https://us05web.zoom.us/j/123456789?pwd=ab".to_string())
        );
        assert_eq!(
            find_meeting_url("here (https://meet.google.com/abc-defg-hij)."),
            Some("https://meet.google.com/abc-defg-hij".to_string())
        );
        // A Calendly link is NOT a meeting/join link.
        assert_eq!(find_meeting_url("https://calendly.com/x/y"), None);
        assert_eq!(find_meeting_url("nope"), None);
    }

    #[tokio::test]
    async fn meeting_link_resolver_env_then_wiki() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_MEETING_LINK", "1");
        let _u = set(
            "AUGMENTAGENT_MEETING_LINK",
            "https://us02web.zoom.us/my/nolan",
        );
        let r = MeetingLinkResolver::new(ResolveCtx::default());
        let f = r
            .try_resolve(&ask("meeting_link", 0.9))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(f.kind, ResolverKind::MeetingLink);
        assert!(f.fill.contains("zoom.us/my/nolan"));

        // Falls back to the wiki index when the env var is unset.
        std::env::remove_var("AUGMENTAGENT_MEETING_LINK");
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("index.md"),
            "# Wiki\n\nStanding room: https://meet.google.com/qrs-tuvw-xyz\n",
        )
        .unwrap();
        let ctx = ResolveCtx {
            wiki_root: Some(d.path().to_path_buf()),
            ..Default::default()
        };
        let r = MeetingLinkResolver::new(ctx);
        let f = r
            .try_resolve(&ask("meeting_link", 0.9))
            .await
            .unwrap()
            .unwrap();
        assert!(f.fill.contains("meet.google.com/qrs-tuvw-xyz"));
    }

    #[tokio::test]
    async fn meeting_link_resolver_noop_without_flag() {
        let _env = env_lock();
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE_MEETING_LINK");
        let _u = set("AUGMENTAGENT_MEETING_LINK", "https://meet.google.com/a-b-c");
        let r = MeetingLinkResolver::new(ResolveCtx::default());
        assert!(r
            .try_resolve(&ask("meeting_link", 0.9))
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn resolver_kind_labels_are_human_readable() {
        assert_eq!(ResolverKind::ShareDoc.label(), "Document link");
        assert_eq!(ResolverKind::MeetingLink.label(), "Video-call link");
        assert_eq!(ResolverKind::Scheduling.label(), "Proposed meeting time");
    }

    #[tokio::test]
    async fn resolve_asks_off_and_shadow_are_empty_outcome() {
        let _env = env_lock();
        let r = reasoner(Some(
            r#"{"asks":[{"text":"x","resolver_kind":"calendly","confidence":0.9}]}"#,
        ));
        let out = resolve_asks(&r, AskResolveMode::Off, "book me", ResolveCtx::default()).await;
        assert_eq!(out, ResolveOutcome::default());
        let r2 = reasoner(Some(
            r#"{"asks":[{"text":"x","resolver_kind":"calendly","confidence":0.9}]}"#,
        ));
        let out2 =
            resolve_asks(&r2, AskResolveMode::Shadow, "book me", ResolveCtx::default()).await;
        assert!(out2.block.is_empty());
        assert!(out2.unresolved.is_empty());
    }

    #[tokio::test]
    async fn resolve_asks_surfaces_unresolved_for_card() {
        let _env = env_lock();
        // share_doc flag ON but NO drive client + NO wiki ⇒ resolver returns
        // Ok(None) ⇒ the ask must surface as "needs your input".
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_SHARE_DOC", "1");
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE_CALENDLY");
        let r = reasoner(Some(
            r#"{"asks":[{"text":"send me the Q4 board deck","resolver_kind":"share_doc","confidence":0.95}]}"#,
        ));
        let out = resolve_asks(
            &r,
            AskResolveMode::Live,
            "Can you send me the Q4 board deck?",
            ResolveCtx::default(),
        )
        .await;
        assert!(out.block.is_empty(), "nothing resolved ⇒ no block");
        assert_eq!(out.unresolved.len(), 1);
        assert_eq!(out.unresolved[0].kind, ResolverKind::ShareDoc);
        assert!(out.unresolved[0].text.contains("Q4 board deck"));
    }

    #[tokio::test]
    async fn resolve_asks_resolved_ask_is_not_in_unresolved() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_CALENDLY", "1");
        let _u = set("AUGMENTAGENT_CALENDLY_URL", "https://calendly.com/n/30");
        let r = reasoner(Some(
            r#"{"asks":[{"text":"got a calendly?","resolver_kind":"calendly","confidence":0.95}]}"#,
        ));
        let out = resolve_asks(
            &r,
            AskResolveMode::Live,
            "Got a Calendly?",
            ResolveCtx::default(),
        )
        .await;
        assert!(out.block.contains("calendly.com/n/30"));
        assert!(
            out.unresolved.is_empty(),
            "a resolved ask must NOT also be surfaced as unresolved"
        );
    }

    #[tokio::test]
    async fn off_path_resolve_outcome_yields_byte_identical_draft_prompt() {
        // The core gating guarantee for #35: with AUGMENTAGENT_ASK_RESOLVE
        // unset/off, the resolve stage produces an empty ResolveOutcome, so
        // `draft_user_message` sees an empty resolved-asks block AND an empty
        // marker — the rendered draft prompt is byte-for-byte the pre-#35
        // output. Asserted here so a future resolver change can't silently
        // perturb the default path.
        let _env = env_lock();
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE");
        let r = reasoner(Some(
            r#"{"asks":[{"text":"got a calendly?","resolver_kind":"calendly","confidence":0.99}]}"#,
        ));
        let out = resolve_asks(
            &r,
            AskResolveMode::from_env(),
            "Hey, got a Calendly?",
            ResolveCtx::default(),
        )
        .await;
        assert_eq!(out, ResolveOutcome::default());
        assert!(out.block.is_empty());
        assert!(out.unresolved.is_empty());

        let email = augmentagent_store::Email {
            message_id: "m1".into(),
            thread_id: Some("t1".into()),
            from: "a@b.com".into(),
            subject: "Re: hi".into(),
            body: "the inbound message".into(),
            date: "2026-05-18T00:00:00Z".into(),
            account_entity_id: Some("acc".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        let with_outcome = crate::prompt::draft_user_message(
            &email, "", "", "", "", &out.block,
        );
        let legacy = crate::prompt::draft_user_message(&email, "", "", "", "", "");
        assert_eq!(with_outcome, legacy);
        assert!(!with_outcome.contains("<resolved_asks>"));
    }

    #[tokio::test]
    async fn resolve_asks_block_wrapper_matches_outcome_block() {
        let _env = env_lock();
        let _g = set("AUGMENTAGENT_ASK_RESOLVE_CALENDLY", "1");
        let _u = set("AUGMENTAGENT_CALENDLY_URL", "https://calendly.com/n/x");
        let body = "Got a Calendly link?";
        let reply =
            r#"{"asks":[{"text":"got a calendly?","resolver_kind":"calendly","confidence":0.9}]}"#;
        let r1 = reasoner(Some(reply));
        let block =
            resolve_asks_block(&r1, AskResolveMode::Live, body, ResolveCtx::default()).await;
        let r2 = reasoner(Some(reply));
        let out = resolve_asks(&r2, AskResolveMode::Live, body, ResolveCtx::default()).await;
        assert_eq!(block, out.block);
    }
}
