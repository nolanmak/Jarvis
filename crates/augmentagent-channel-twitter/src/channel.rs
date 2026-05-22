//! Trigger implementations for the X / Twitter channel.
//!
//! - **#15 `TwitterFeedTrigger`** — implements [`Trigger`] directly. Each tick
//!   it walks the wiki's `people/*.md` pages, keeps only those marked
//!   `close: true` in front-matter AND carrying a `twitter:` identity, then
//!   pulls each close friend's recent tweets since the last seen id. New
//!   tweets become `WorkItem { platform: "twitter", kind: "post_engagement" }`.
//!   The triage → draft → Discord-approval pipeline (driven by the CLI's
//!   ChannelRunner, same as LinkedIn) decides whether to draft a reply; no
//!   tweet is ever auto-posted — replies go through Discord approval.
//!   Cadence: 2h base ± up to 20min jitter; a daily reply cap is enforced by
//!   the shared [`RateGovernor`] (`Platform::Twitter` / `ActionKind::Reply`).
//!
//! - **#16 `TwitterDmTrigger`** — implements [`InboundSource`] (wrapped by
//!   [`InboundMessageTrigger`]). Polls the DM inbox no more often than every
//!   30min. Inbound DMs become `WorkItem { platform: "twitter", kind: "dm" }`.
//!   Shares the same `TwitterApi` session as the feed trigger.
//!
//! - **#56 / I10 `TwitterChannel`** — per-message triage → draft pipeline
//!   for the WorkItems above. Draft step is **code-mode by default** (I6/I7
//!   pattern, lifted from `augmentagent-channel-email`), with classic prose
//!   draft as the self-repair fallback. The terminal `tools.draft` call lands
//!   the `actions` row with `mode='code'` and `channel='twitter'`; the same
//!   row carries the `generatedSource` + `toolCallTrace` audit trail. Twitter's
//!   15/day quota + tight rate-governor are NOT re-gated here — they already
//!   route through `DefaultDispatcher::draft → RateGovernor::permit`, and the
//!   arming flag (`AUGMENTAGENT_TWITTER_REAL_ENABLED`, issue #32 / PR #40)
//!   stays at `CreateTweetClient::create` so the live wire call remains
//!   gated whether the draft came from code-mode or classic.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::{ApprovalBroker, NoopBroker};
use augmentagent_channel_core::code_mode::{
    self, handle_code_mode_failure, manifest_v1, report_classic_fallback, DefaultDispatcher,
    DraftOutcome, FailureCtx, FailureStage, GhCliIssueRunner, GhIssueRunner, MessageContext,
};
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::prompt::{
    code_mode_system, code_mode_user_message, draft_user_message, triage_user_message,
};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::{InboundSource, Trigger, WorkItem, WorkItemHandler};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Store, TriageResult, NUDGE_INTERVAL_MS};
use augmentagent_wiki::{IdentityIndex, WikiLayout};

use crate::api::{TwitterApi, TwitterError};
use crate::types::{Tweet, TwitterDm};

/// Feed-poll base cadence: 12×/day. X anti-automation cares about request
/// rhythm; 2h ± jitter stays well inside human range.
pub const FEED_POLL_SECS: u64 = 2 * 60 * 60;
/// Jitter half-window: ±20min around the base interval.
pub const FEED_JITTER_SECS: u64 = 20 * 60;
/// DM poll floor — never poll the inbox more often than this.
pub const DM_POLL_MIN_SECS: u64 = 30 * 60;

/// A close friend resolved from the wiki: their twitter id + a stable slug
/// for last-seen bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFriend {
    pub slug: String,
    pub twitter_id: String,
}

/// Walk `people/*.md` and return the set of close friends with a `twitter:`
/// identity. "Close" = a `close: true` line inside the page's YAML
/// front-matter. The identity (`identities.twitter`) is parsed by the wiki
/// crate's [`IdentityIndex`]; the `close` flag is a lightweight scan because
/// the wiki front-matter struct doesn't model it.
pub fn close_friends_with_twitter(layout: &WikiLayout) -> std::io::Result<Vec<CloseFriend>> {
    let index = IdentityIndex::build(layout)?;
    let mut out = Vec::new();
    for page in index.pages() {
        let Some(tw) = page.identities.twitter.as_deref() else {
            continue;
        };
        if tw.is_empty() {
            continue;
        }
        if page_is_close(&page.path) {
            out.push(CloseFriend {
                slug: page.slug.clone(),
                twitter_id: tw.to_string(),
            });
        }
    }
    Ok(out)
}

/// True iff the page's front-matter contains a `close: true` line. Cheap
/// scan of the YAML block only (between the leading `---` fences) so a stray
/// "close" in body prose can't trip it.
fn page_is_close(path: &std::path::Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(after) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return false;
    };
    for line in after.lines() {
        let t = line.trim_end();
        if t == "---" {
            return false; // end of front-matter, no close flag seen
        }
        let l = t.trim();
        if l == "close: true" || l == "close: yes" {
            return true;
        }
    }
    false
}

/// Per-friend last-seen tweet id, so successive ticks don't re-yield. In
/// memory only — the triage pipeline's `emails` dedup (by `message_id`) is
/// the durable backstop across restarts.
type SeenMap = std::collections::HashMap<String, String>;

/// #15 — friend-post engagement trigger.
pub struct TwitterFeedTrigger<A: TwitterApi> {
    api: Arc<A>,
    wiki_root: PathBuf,
    my_user_id: String,
    seen: Mutex<SeenMap>,
}

impl<A: TwitterApi> TwitterFeedTrigger<A> {
    pub fn new(api: Arc<A>, wiki_root: PathBuf, my_user_id: String) -> Self {
        Self {
            api,
            wiki_root,
            my_user_id,
            seen: Mutex::new(SeenMap::new()),
        }
    }

    /// One poll pass: resolve close friends, pull each one's new tweets.
    pub async fn poll(&self) -> anyhow::Result<Vec<WorkItem>> {
        let layout = WikiLayout::new(self.wiki_root.clone());
        let friends = match close_friends_with_twitter(&layout) {
            Ok(f) => f,
            Err(e) => {
                warn!("twitter feed: wiki scan failed: {e}");
                return Ok(Vec::new());
            }
        };
        if friends.is_empty() {
            debug!("twitter feed: no close friends with a twitter: identity");
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let mut seen = self.seen.lock().await;
        for f in friends {
            let since = seen.get(&f.twitter_id).cloned();
            let tweets = match self
                .api
                .fetch_user_tweets(&f.twitter_id, since.as_deref())
                .await
            {
                Ok(t) => t,
                Err(TwitterError::AuthExpired) => {
                    warn!("twitter auth expired — run `augmentagent twitter login`");
                    return Ok(items);
                }
                Err(e) => {
                    warn!(friend = %f.slug, "twitter feed fetch failed: {e}");
                    continue;
                }
            };
            // Advance the high-water mark to the newest id we saw.
            if let Some(max) = max_id(&tweets) {
                seen.insert(f.twitter_id.clone(), max);
            }
            for t in tweets {
                if t.is_own(&self.my_user_id) {
                    continue;
                }
                items.push(tweet_to_work_item(t, &self.my_user_id));
            }
        }
        info!(count = items.len(), "twitter feed poll complete");
        Ok(items)
    }

    /// Base poll interval. Channel-runner adds jitter on top.
    pub fn poll_interval() -> Duration {
        let secs = std::env::var("AUGMENTAGENT_TWITTER_FEED_POLL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s >= 60)
            .unwrap_or(FEED_POLL_SECS);
        Duration::from_secs(secs)
    }
}

#[async_trait]
impl<A: TwitterApi + 'static> Trigger for TwitterFeedTrigger<A> {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        self.poll().await
    }
}

/// #16 — DM inbound source. Wrapped by `InboundMessageTrigger`.
pub struct TwitterDmSource<A: TwitterApi> {
    api: Arc<A>,
    my_user_id: String,
    last_poll_ms: Mutex<i64>,
    seen: Mutex<std::collections::HashSet<String>>,
}

impl<A: TwitterApi> TwitterDmSource<A> {
    pub fn new(api: Arc<A>, my_user_id: String) -> Self {
        Self {
            api,
            my_user_id,
            last_poll_ms: Mutex::new(0),
            seen: Mutex::new(std::collections::HashSet::new()),
        }
    }

    fn now_ms() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

#[async_trait]
impl<A: TwitterApi + 'static> InboundSource for TwitterDmSource<A> {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        // Self-throttle: never hit the inbox more than once per 30min even
        // if the runner ticks faster.
        {
            let mut last = self.last_poll_ms.lock().await;
            let now = Self::now_ms();
            if *last != 0 && now - *last < (DM_POLL_MIN_SECS as i64) * 1000 {
                debug!("twitter dm: skipping poll (under 30min floor)");
                return Ok(Vec::new());
            }
            *last = now;
        }

        let dms = match self.api.fetch_dm_inbox(None).await {
            Ok(d) => d,
            Err(TwitterError::AuthExpired) => {
                warn!("twitter auth expired — run `augmentagent twitter login`");
                return Ok(Vec::new());
            }
            Err(e) => {
                warn!("twitter dm inbox fetch failed: {e}");
                return Ok(Vec::new());
            }
        };

        let mut seen = self.seen.lock().await;
        let mut items = Vec::new();
        for dm in dms {
            if dm.is_outbound(&self.my_user_id) {
                continue;
            }
            if !seen.insert(dm.event_id.clone()) {
                continue; // already yielded this run-lifetime
            }
            items.push(dm_to_work_item(dm, &self.my_user_id));
        }
        info!(count = items.len(), "twitter dm poll complete");
        Ok(items)
    }
}

fn max_id(tweets: &[Tweet]) -> Option<String> {
    tweets
        .iter()
        .max_by(|a, b| {
            match (a.rest_id.parse::<u128>(), b.rest_id.parse::<u128>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => a.rest_id.cmp(&b.rest_id),
            }
        })
        .map(|t| t.rest_id.clone())
}

fn tweet_to_work_item(t: Tweet, my_user_id: &str) -> WorkItem {
    let email = t.clone().into_email(my_user_id);
    WorkItem {
        platform: "twitter".into(),
        kind: "post_engagement".into(),
        external_id: email.message_id.clone(),
        payload: serde_json::to_value(&email).unwrap_or(serde_json::Value::Null),
    }
}

fn dm_to_work_item(dm: TwitterDm, my_user_id: &str) -> WorkItem {
    let email = dm.clone().into_email(my_user_id);
    WorkItem {
        platform: "twitter".into(),
        kind: "dm".into(),
        external_id: email.message_id.clone(),
        payload: serde_json::to_value(&email).unwrap_or(serde_json::Value::Null),
    }
}

// =============================================================================
// #56 / I10 — TwitterChannel: triage + code-mode draft pipeline
// =============================================================================
//
// Per-message dispatch for the WorkItems the triggers above emit. The shape
// mirrors `LinkedInChannel::process_email` (no server-side draft — approval
// card posts directly; the approver sends via `CreateTweetClient` on click),
// with the `Reply` arm lifted from `augmentagent-channel-email`'s post-I7
// code-mode block verbatim — same call_code_mode → run_program → self-repair
// → classic fallback shape, only the dispatcher's `MessageContext.channel`
// flips from `"gmail"` to `"twitter"`.

/// Outcome of one inbound tweet/DM through [`TwitterChannel::process_email`].
/// Mirrors the [`LinkedInChannel`-style] enum so a future `ChannelRunner`
/// hookup can fold counts the same way.
#[derive(Debug, Clone, Copy)]
pub enum DispatchOutcome {
    Skipped,
    Flagged,
    DryRun,
    /// Draft computed, approval card posted; approver sends via
    /// [`crate::CreateTweetClient`] on click. The `twitter_armed` gate
    /// still fires at send time inside `CreateTweetClient::create` —
    /// code-mode does NOT bypass it.
    AwaitingApproval,
}

#[derive(Clone, Debug)]
pub struct TwitterChannelConfig {
    pub dry_run: bool,
    /// Wiki root for the `wiki.draftHint` lookup the code-mode dispatcher
    /// uses (and the classic-fallback draft prompt's hint block).
    pub wiki_root: Option<PathBuf>,
}

impl Default for TwitterChannelConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            wiki_root: None,
        }
    }
}

/// Per-message dispatcher for the X / Twitter channel.
///
/// Construct once at daemon start, then drive each tweet/DM through
/// [`TwitterChannel::process_email`] (the unified entry point, mirroring
/// `LinkedInChannel::process_email`). Outbound filtering + `into_email`
/// happens upstream in the triggers — by the time we get an `Email` here
/// it's already known to be inbound and platform-tagged `"twitter"`.
pub struct TwitterChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: TwitterChannelConfig,
    /// gh CLI runner for I7 postmortems. Defaults to [`GhCliIssueRunner`];
    /// tests pass a recording stub via [`TwitterChannel::with_gh_issue_runner`].
    gh_issue_runner: Arc<dyn GhIssueRunner>,
}

impl<R: Reasoner + 'static> TwitterChannel<R> {
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        config: TwitterChannelConfig,
    ) -> Self {
        Self {
            store,
            reasoner,
            approvals,
            config,
            gh_issue_runner: Arc::new(GhCliIssueRunner::new()),
        }
    }

    /// Dry-run constructor wired to a `NoopBroker` (parity with
    /// `GmailChannel::dry_run` / `LinkedInChannel::dry_run`).
    pub fn dry_run(
        store: Arc<Store>,
        reasoner: Arc<R>,
        wiki_root: Option<PathBuf>,
    ) -> Self {
        Self::new(
            store,
            reasoner,
            Arc::new(NoopBroker),
            TwitterChannelConfig {
                dry_run: true,
                wiki_root,
            },
        )
    }

    /// Swap the gh-CLI runner used for I7 postmortem issues. Tests pass a
    /// recording stub so the suite never files real issues.
    pub fn with_gh_issue_runner(mut self, runner: Arc<dyn GhIssueRunner>) -> Self {
        self.gh_issue_runner = runner;
        self
    }

    /// Run one inbound tweet/DM `Email` through triage → code-mode draft →
    /// (on failure) self-repair → (on repair failure) classic prose draft →
    /// approval card. The channel string passed to the code-mode dispatcher
    /// is `"twitter"` so the `actions` row carries `channel='twitter'` and
    /// `tools.draft` routes through the Twitter rate-governor.
    pub async fn process_email(
        &self,
        email: augmentagent_store::Email,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(None);
        }
        if self.store.is_message_processed(&email.message_id)? {
            return Ok(None);
        }

        // --- TRIAGE (Opus, optional wiki read) ---
        let triage_opts_ = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, "", "");
        let raw = self.reasoner.call(&triage_opts_, &triage_prompt).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "triage parse failed: {e}; raw={raw}");
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Error,
                )?;
                return Err(e.into());
            }
        };

        match decision.decision {
            DecisionKind::Skip => {
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Skipped,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                Ok(Some(DispatchOutcome::Skipped))
            }
            DecisionKind::Flag => {
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Flagged,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                let reason = decision.reason.as_deref().unwrap_or("flagged");
                if let Err(e) = self.approvals.post_flag_notice(&email, reason).await {
                    warn!(message_id = %email.message_id, "post_flag_notice failed: {e}");
                }
                Ok(Some(DispatchOutcome::Flagged))
            }
            DecisionKind::Reply => self.dispatch_reply_arm(email).await,
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "twitter triage returned non-message decision kind; treating as skip"
                );
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                Ok(Some(DispatchOutcome::Skipped))
            }
        }
    }

    /// The `DecisionKind::Reply` branch. Pulled into its own helper to keep
    /// `process_email` skim-friendly: the code-mode block + self-repair +
    /// classic fallback is the bulk of the per-message work.
    async fn dispatch_reply_arm(
        &self,
        email: augmentagent_store::Email,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        // Wiki hint: precompute once so both the code-mode dispatcher's
        // `wiki.draftHint` tool and the classic-fallback prompt see the
        // same context (parity with email channel).
        let wiki_hint = self
            .config
            .wiki_root
            .as_ref()
            .map(|root| {
                let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                augmentagent_wiki::WikiReader::new(&layout).draft_hint(&email)
            })
            .unwrap_or_default();

        // --- 2a. CODE-MODE attempt (mirrors email/post-I7 block) ---
        //
        // Step 1: ask the reasoner for a TypeScript program that orchestrates
        // tool calls and ends with `tools.draft("twitter", body, reason)`.
        // Step 2: run the program in the Deno sandbox. The dispatcher's
        // terminal `tools.draft` handler writes the `actions` row with
        // `mode='code'`, `channel='twitter'`, `generatedSource`, and
        // `toolCallTrace`, then stashes the action id for the post-success
        // pickup below.
        //
        // On ANY failure (reasoner spawn, missing fenced block, sandbox
        // timeout, runtime exception, dispatcher error) we hand off to
        // `handle_code_mode_failure` (I7) which runs one self-repair pass.
        // If repair lands a working code-mode draft we use it; otherwise we
        // fall through to the classic prompt path AND, after it lands its
        // row, call `report_classic_fallback` to file the postmortem gh
        // issue + post the Discord notice.
        let manifest = manifest_v1();
        let system_prompt = code_mode_system(&manifest);
        // Empty tone/thread/archetype/resolve blocks — Twitter doesn't run
        // those Phase-2/3/5 pre-resolvers (yet); the prompt accepts empty
        // strings as "no injection" so the bytes degrade cleanly.
        let user_msg = code_mode_user_message(&email, &wiki_hint, "", "", "", "");
        let code_mode_opts = augmentagent_channel_core::ReasonerOpts {
            system_prompt,
            model: None,
            allowed_tools: Vec::new(),
            add_dirs: Vec::new(),
            permission_mode: "default".into(),
            cwd: None,
            env: Vec::new(),
        };
        let message_ctx = MessageContext {
            channel: "twitter".to_string(),
            email: email.clone(),
            account_id: email.account_entity_id.clone(),
        };

        let mut cm_source: String = String::new();
        let cm_attempt: Result<String, (code_mode::CodeModeError, FailureStage)> = async {
            let ts_source = match self.reasoner.call_code_mode(&code_mode_opts, &user_msg).await {
                Ok(s) => s,
                Err(e) => {
                    let cme = match e.downcast::<code_mode::CodeModeError>() {
                        Ok(cme) => cme,
                        Err(other) => code_mode::CodeModeError::ReasonerFailed(other),
                    };
                    return Err((cme, FailureStage::CallCodeMode));
                }
            };
            cm_source = ts_source.clone();
            let dispatcher = DefaultDispatcher::new(
                self.store.as_ref(),
                message_ctx.clone(),
                ts_source.clone(),
            )
            .with_wiki_hint(wiki_hint.clone());
            if let Err(e) = code_mode::run_program(&ts_source, &manifest, &dispatcher).await {
                let wrapped = code_mode::CodeModeError::ReasonerFailed(anyhow::anyhow!(
                    "run_program: {e}"
                ));
                return Err((wrapped, FailureStage::RunProgram));
            }
            match dispatcher.last_action_id() {
                Some(id) => Ok(id),
                None => Err((
                    code_mode::CodeModeError::ReasonerFailed(anyhow::anyhow!(
                        "code-mode program produced no draft call"
                    )),
                    FailureStage::RunProgram,
                )),
            }
        }
        .await;

        // Either Some(action_id) → keep going on the code-mode rail, or
        // None + carried FailureRecord → run classic, then report.
        let (code_mode_action_id, pending_classic_record): (
            Option<String>,
            Option<code_mode::FailureRecord>,
        ) = match cm_attempt {
            Ok(action_id) => (Some(action_id), None),
            Err((cme, stage)) => {
                warn!(
                    message_id = %email.message_id,
                    stage = ?stage,
                    "code-mode attempt failed: {cme}; invoking self-repair"
                );
                let failure_ctx = FailureCtx {
                    reasoner: self.reasoner.as_ref(),
                    opts: code_mode_opts.clone(),
                    user_msg: user_msg.clone(),
                    manifest: manifest.clone(),
                    message_ctx: message_ctx.clone(),
                    wiki_hint: wiki_hint.clone(),
                    store: Arc::clone(&self.store),
                    broker: Arc::clone(&self.approvals),
                    gh: Arc::clone(&self.gh_issue_runner),
                    email: email.clone(),
                    channel: "twitter".to_string(),
                    model: code_mode_opts.model.clone(),
                    manifest_version: "v1",
                };
                match handle_code_mode_failure(&failure_ctx, &cm_source, &cme, stage).await {
                    DraftOutcome::CodeMode {
                        action_id,
                        repair_used,
                    } => {
                        info!(
                            message_id = %email.message_id,
                            action_id = %action_id,
                            repair_used,
                            "code-mode self-repair succeeded"
                        );
                        (Some(action_id), None)
                    }
                    DraftOutcome::ClassicNeeded(record) => {
                        error!(
                            message_id = %email.message_id,
                            "code-mode self-repair failed; falling back to classic"
                        );
                        (None, Some(record))
                    }
                }
            }
        };

        // --- 2b. Code-mode success path ---
        // The dispatcher already wrote the actions row with mode='code'.
        // Read the draft body back out and post the approval card. No
        // server-side draft step (Twitter has none — like LinkedIn).
        if let Some(action_id) = code_mode_action_id {
            let draft_body = self
                .store
                .get_action_with_email(&action_id)?
                .and_then(|a| a.action.draft_body)
                .unwrap_or_default();

            if self.config.dry_run {
                // Promote the dispatcher's `Pending` row to `DryRun` so
                // dry-run accounting matches classic. The persisted code-mode
                // columns (mode='code', generatedSource, toolCallTrace) are
                // untouched.
                self.store.update_action_status(
                    &action_id,
                    ActionStatus::DryRun,
                    Some(&draft_body),
                    None,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                println!(
                    "[twitter reply dry-run:code] {} from={}\n--- draft ---\n{}\n--- /draft ---",
                    email.message_id, email.from, draft_body,
                );
                return Ok(Some(DispatchOutcome::DryRun));
            }
            return self
                .post_approval_for(action_id, email, draft_body)
                .await;
        }

        // --- 2c. Classic fallback (I7) ---
        // Reached when code-mode failed AND self-repair didn't produce a
        // working code-mode draft. Same shape as the pre-code-mode classic
        // prompt path — Twitter has no skill-dir / SKILL.md so the system
        // prompt is empty (mirrors `LinkedInChannel`).
        let draft_opts_ = draft_opts(String::new(), self.config.wiki_root.clone());
        let draft_prompt = draft_user_message(&email, &wiki_hint, "", "", "", "");
        let draft = match self.reasoner.call(&draft_opts_, &draft_prompt).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                error!(message_id = %email.message_id, "classic draft call failed: {e}");
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Error,
                )?;
                return Err(e);
            }
        };

        if self.config.dry_run {
            let action_id = self.store.log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some(&draft),
                ActionStatus::DryRun,
            )?;
            self.store
                .mark_email_processed(&email.message_id, TriageResult::Reply)?;
            println!(
                "[twitter reply dry-run] {} from={}\n--- draft ---\n{}\n--- /draft ---",
                email.message_id, email.from, draft,
            );
            // I7: file postmortem when classic fallback was triggered by a
            // code-mode failure. Successful repair never reaches here (it
            // returns Some(action_id) above).
            if let Some(record) = pending_classic_record {
                let failure_ctx = FailureCtx {
                    reasoner: self.reasoner.as_ref(),
                    opts: code_mode_opts.clone(),
                    user_msg: user_msg.clone(),
                    manifest: manifest.clone(),
                    message_ctx: message_ctx.clone(),
                    wiki_hint: wiki_hint.clone(),
                    store: Arc::clone(&self.store),
                    broker: Arc::clone(&self.approvals),
                    gh: Arc::clone(&self.gh_issue_runner),
                    email: email.clone(),
                    channel: "twitter".to_string(),
                    model: code_mode_opts.model.clone(),
                    manifest_version: "v1",
                };
                report_classic_fallback(&failure_ctx, &record, &action_id).await;
            }
            return Ok(Some(DispatchOutcome::DryRun));
        }

        // Non-dry-run: pre-create the row in `Pending` so we can pass the
        // action id to `report_classic_fallback` before handing off to the
        // approval flow.
        let classic_action_id = self.store.log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some(&draft),
            ActionStatus::Pending,
        )?;
        if let Some(record) = pending_classic_record.as_ref() {
            let failure_ctx = FailureCtx {
                reasoner: self.reasoner.as_ref(),
                opts: code_mode_opts.clone(),
                user_msg: user_msg.clone(),
                manifest: manifest.clone(),
                message_ctx: message_ctx.clone(),
                wiki_hint: wiki_hint.clone(),
                store: Arc::clone(&self.store),
                broker: Arc::clone(&self.approvals),
                gh: Arc::clone(&self.gh_issue_runner),
                email: email.clone(),
                channel: "twitter".to_string(),
                model: code_mode_opts.model.clone(),
                manifest_version: "v1",
            };
            report_classic_fallback(&failure_ctx, record, &classic_action_id).await;
        }
        self.post_approval_for(classic_action_id, email, draft).await
    }

    /// Post the approval card (Twitter has no server-side draft — the
    /// approver sends via [`crate::CreateTweetClient`] on click, which is
    /// where the arming gate + 15/day quota live). Mirrors the
    /// `DecisionKind::Reply` tail of `LinkedInChannel::process_email`.
    async fn post_approval_for(
        &self,
        action_id: String,
        email: augmentagent_store::Email,
        draft: String,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        // Ensure the row carries the latest draft body (code-mode path
        // already wrote it via the dispatcher, but the classic-fallback path
        // pre-created Pending with `draft` — both converge here).
        self.store.update_action_status(
            &action_id,
            ActionStatus::Pending,
            Some(&draft),
            None,
        )?;
        if let Err(e) = self
            .approvals
            .post_approval(&action_id, &email, &draft)
            .await
        {
            self.store.update_action_status(
                &action_id,
                ActionStatus::Error,
                None,
                Some(&format!("post_approval: {e}")),
            )?;
            return Err(anyhow::anyhow!("post_approval: {e}"));
        }
        if let Err(e) = self
            .store
            .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
        {
            warn!(action_id, "record_nudge after post_approval failed: {e}");
        }
        self.store
            .mark_email_processed(&email.message_id, TriageResult::Reply)?;
        info!(action_id, message_id = %email.message_id, "twitter approval card posted");
        Ok(Some(DispatchOutcome::AwaitingApproval))
    }
}

/// `WorkItemHandler` for the #25 `ChannelRunner` cutover. Rehydrates each
/// `WorkItem` payload back into an `Email` and feeds it through
/// [`TwitterChannel::process_email`] — the identical triage → draft →
/// approve path either driver would run. Errors are logged + swallowed so
/// one bad message never aborts a tick (mirroring `GmailWorkHandler` /
/// `LinkedInWorkHandler`).
pub struct TwitterWorkHandler<R: Reasoner + 'static> {
    channel: Arc<TwitterChannel<R>>,
}

impl<R: Reasoner + 'static> TwitterWorkHandler<R> {
    pub fn new(channel: Arc<TwitterChannel<R>>) -> Self {
        Self { channel }
    }
}

#[async_trait]
impl<R: Reasoner + 'static> WorkItemHandler for TwitterWorkHandler<R> {
    async fn handle(&self, item: WorkItem) -> anyhow::Result<()> {
        let email: augmentagent_store::Email = serde_json::from_value(item.payload)
            .map_err(|e| anyhow::anyhow!("twitter work item payload not an Email: {e}"))?;
        match self.channel.process_email(email).await {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("twitter handle (channel-runner): process_email failed: {e:#}");
                Ok(())
            }
        }
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
    use std::sync::Mutex as StdMutex;

    struct StubApi {
        tweets: StdMutex<Vec<Tweet>>,
        dms: StdMutex<Vec<TwitterDm>>,
        expired: bool,
    }
    impl StubApi {
        fn with(tweets: Vec<Tweet>, dms: Vec<TwitterDm>) -> Self {
            Self {
                tweets: StdMutex::new(tweets),
                dms: StdMutex::new(dms),
                expired: false,
            }
        }
        fn expired() -> Self {
            Self {
                tweets: StdMutex::new(vec![]),
                dms: StdMutex::new(vec![]),
                expired: true,
            }
        }
    }
    #[async_trait]
    impl TwitterApi for StubApi {
        async fn fetch_user_tweets(
            &self,
            _u: &str,
            since: Option<&str>,
        ) -> Result<Vec<Tweet>, TwitterError> {
            if self.expired {
                return Err(TwitterError::AuthExpired);
            }
            let all = self.tweets.lock().unwrap().clone();
            Ok(match since {
                None => all,
                Some(s) => all
                    .into_iter()
                    .filter(|t| t.rest_id.as_str() > s)
                    .collect(),
            })
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
            if self.expired {
                return Err(TwitterError::AuthExpired);
            }
            Ok(self.dms.lock().unwrap().clone())
        }
        async fn send_dm(&self, _c: &str, _t: &str) -> Result<String, TwitterError> {
            Ok("evt".into())
        }
    }

    fn wiki_with(pages: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        for (slug, fm) in pages {
            let body = format!("---\n{fm}\n---\n\n# {slug}\n");
            std::fs::write(layout.people_dir().join(format!("{slug}.md")), body).unwrap();
        }
        let root = d.path().to_path_buf();
        (d, root)
    }

    fn tweet(id: &str, author_id: &str) -> Tweet {
        Tweet {
            rest_id: id.into(),
            conversation_id: id.into(),
            author_name: "Jane".into(),
            author_handle: "jane".into(),
            author_id: author_id.into(),
            text: format!("tweet {id}"),
            created_at_ms: 1,
        }
    }

    #[test]
    fn close_friends_filters_on_flag_and_identity() {
        let (_d, root) = wiki_with(&[
            (
                "jane",
                "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
            ),
            (
                "bob",
                "kind: person\nkey: bob\nidentities:\n  twitter: \"66\"",
            ), // not close
            (
                "amy",
                "kind: person\nkey: amy\nclose: true", // close but no twitter
            ),
        ]);
        let layout = WikiLayout::new(root);
        let friends = close_friends_with_twitter(&layout).unwrap();
        assert_eq!(friends.len(), 1);
        assert_eq!(friends[0].slug, "jane");
        assert_eq!(friends[0].twitter_id, "55");
    }

    #[test]
    fn page_is_close_only_scans_front_matter() {
        let (_d, root) = wiki_with(&[(
            "x",
            "kind: person\nkey: x\nidentities:\n  twitter: \"1\"",
        )]);
        // body says "close: true" but not in front-matter
        let p = root.join("people/x.md");
        let mut body = std::fs::read_to_string(&p).unwrap();
        body.push_str("\nthe deal will close: true tomorrow\n");
        std::fs::write(&p, body).unwrap();
        assert!(!page_is_close(&p));
    }

    #[tokio::test]
    async fn feed_trigger_yields_new_tweets_and_skips_own() {
        let (_d, root) = wiki_with(&[(
            "jane",
            "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
        )]);
        let api = Arc::new(StubApi::with(
            vec![tweet("200", "55"), tweet("300", "99")],
            vec![],
        ));
        let tr = TwitterFeedTrigger::new(api, root, "99".into());
        let cancel = CancellationToken::new();
        let items = tr.next_work_items(&cancel).await.unwrap();
        // own tweet (author 99) skipped; only the "55" one yields
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].platform, "twitter");
        assert_eq!(items[0].kind, "post_engagement");
        assert_eq!(items[0].external_id, "200");
    }

    #[tokio::test]
    async fn feed_trigger_dedups_via_seen_high_water_mark() {
        let (_d, root) = wiki_with(&[(
            "jane",
            "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
        )]);
        let api = Arc::new(StubApi::with(vec![tweet("200", "55")], vec![]));
        let tr = TwitterFeedTrigger::new(api, root, "99".into());
        let cancel = CancellationToken::new();
        assert_eq!(tr.next_work_items(&cancel).await.unwrap().len(), 1);
        // Second tick: same single tweet, now <= seen high-water mark → none.
        assert_eq!(tr.next_work_items(&cancel).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn feed_trigger_auth_expired_returns_empty_not_err() {
        let (_d, root) = wiki_with(&[(
            "jane",
            "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
        )]);
        let api = Arc::new(StubApi::expired());
        let tr = TwitterFeedTrigger::new(api, root, "99".into());
        let cancel = CancellationToken::new();
        assert!(tr.next_work_items(&cancel).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn dm_source_yields_inbound_skips_outbound() {
        let dms = vec![
            TwitterDm {
                event_id: "e1".into(),
                conversation_id: "55-99".into(),
                sender_name: "Jane".into(),
                sender_handle: "jane".into(),
                sender_id: "55".into(),
                text: "hi".into(),
                created_at_ms: 1,
            },
            TwitterDm {
                event_id: "e2".into(),
                conversation_id: "55-99".into(),
                sender_name: "Me".into(),
                sender_handle: "me".into(),
                sender_id: "99".into(),
                text: "my own".into(),
                created_at_ms: 2,
            },
        ];
        let api = Arc::new(StubApi::with(vec![], dms));
        let src = TwitterDmSource::new(api, "99".into());
        let items = src.fetch_new().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "dm");
        assert_eq!(items[0].external_id, "e1");
    }

    #[tokio::test]
    async fn dm_source_throttles_to_30min_floor() {
        let dms = vec![TwitterDm {
            event_id: "e1".into(),
            conversation_id: "c".into(),
            sender_name: "Jane".into(),
            sender_handle: "jane".into(),
            sender_id: "55".into(),
            text: "hi".into(),
            created_at_ms: 1,
        }];
        let api = Arc::new(StubApi::with(vec![], dms));
        let src = TwitterDmSource::new(api, "99".into());
        assert_eq!(src.fetch_new().await.unwrap().len(), 1);
        // Immediate re-poll is throttled (under 30min floor) → empty.
        assert!(src.fetch_new().await.unwrap().is_empty());
    }

    // =========================================================================
    // #56 / I10 — TwitterChannel code-mode + classic-fallback tests
    // =========================================================================

    use augmentagent_channel_core::{Reasoner, ReasonerOpts};
    use augmentagent_store::Email;

    /// Module-load env-var gate so any test that traverses the I7
    /// code-mode-failure → classic fallback path can't spawn the real `gh`
    /// CLI. Mirrors the email channel's `disable_gh_for_tests` helper.
    static GH_DISABLE_INIT: std::sync::Once = std::sync::Once::new();
    fn disable_gh_for_tests() {
        GH_DISABLE_INIT.call_once(|| {
            // SAFETY: set once at module init before any test runs; no
            // concurrent reads of this var.
            std::env::set_var("AUGMENTAGENT_GH_DISABLE", "1");
        });
    }

    /// Scripted reasoner — returns canned responses in order per `call`. The
    /// `call_code_mode` / `call_code_mode_with_repair` defaults (from the
    /// trait) reuse `call` after adding the `extract_ts_block` step, so a
    /// scripted response that omits a fenced block reliably produces a
    /// `NoCodeBlock` error — exactly what the I7 fallback tests need.
    struct ScriptedReasoner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }
    impl ScriptedReasoner {
        fn new<I: IntoIterator<Item = &'static str>>(resps: I) -> Self {
            Self {
                responses: std::sync::Mutex::new(resps.into_iter().map(String::from).collect()),
            }
        }
    }
    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, _opts: &ReasonerOpts, _u: &str) -> anyhow::Result<String> {
            let mut q = self.responses.lock().unwrap();
            Ok(q.pop_front()
                .unwrap_or_else(|| "{\"decision\":\"skip\",\"reason\":\"stub\"}".into()))
        }
    }

    /// Recording gh runner — captures every issue create without shelling
    /// out, returning canned numbers starting at `start`. Lets the
    /// fallback test assert exactly one postmortem was filed.
    #[derive(Default)]
    struct RecordingGh {
        calls: std::sync::Mutex<Vec<(String, String, Vec<String>)>>,
        next_number: std::sync::Mutex<u64>,
    }
    impl RecordingGh {
        fn new(start: u64) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                next_number: std::sync::Mutex::new(start),
            }
        }
    }
    #[async_trait]
    impl GhIssueRunner for RecordingGh {
        async fn create_issue(
            &self,
            title: &str,
            body: &str,
            labels: &[&str],
        ) -> anyhow::Result<u64> {
            self.calls.lock().unwrap().push((
                title.to_string(),
                body.to_string(),
                labels.iter().map(|s| s.to_string()).collect(),
            ));
            let mut n = self.next_number.lock().unwrap();
            let out = *n;
            *n += 1;
            Ok(out)
        }
    }

    /// Recording approval broker — captures `post_approval` + `post_flag_notice`
    /// calls so the fallback test can assert "approval card still posted via
    /// classic" + "Discord notice fired with the gh issue number".
    #[derive(Default)]
    struct RecordingBroker {
        posts: std::sync::Mutex<Vec<String>>,
        flag_posts: std::sync::Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(
            &self,
            action_id: &str,
            _email: &Email,
            _draft: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.posts.lock().unwrap().push(action_id.to_string());
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            email: &Email,
            reason: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.flag_posts
                .lock()
                .unwrap()
                .push((email.message_id.clone(), reason.to_string()));
            Ok(())
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        disable_gh_for_tests();
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    originalBody TEXT, draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                    createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    body TEXT, receivedAt TEXT, accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn tweet_email(message_id: &str) -> Email {
        Email {
            message_id: message_id.to_string(),
            thread_id: Some(message_id.to_string()),
            from: "Jane <twitter:55>".into(),
            subject: "[X post by @jane]".into(),
            body: "any update?".into(),
            date: "2026-05-22T00:00:00Z".into(),
            account_entity_id: Some("twitter:99".into()),
            platform: "twitter".into(),
            kind: "post_engagement".into(),
        }
    }

    fn deno_available_for_tests() -> bool {
        // Mirror the email crate: code-mode happy-path tests need `deno` to
        // actually run the sandbox; skip cleanly when it's missing on CI/dev.
        let bin = std::env::var("AUGMENTAGENT_DENO_BIN")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "deno".to_string());
        std::process::Command::new(&bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn skip_decision_logs_skipped_action() {
        // Triage → skip is a pure happy path that doesn't touch code-mode at
        // all — just confirms the pipeline shape (upsert → triage → log_action
        // → mark_email_processed) is wired correctly.
        let (store, _f) = tmp_store();
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"promo"}"#,
        ]));
        let ch = TwitterChannel::dry_run(store.clone(), reasoner, None);
        let out = ch.process_email(tweet_email("t-skip")).await.unwrap();
        assert!(matches!(out, Some(DispatchOutcome::Skipped)));
        // emails row marked as processed (triage=skip) so a re-poll won't
        // re-triage the same tweet.
        let processed: bool = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT agentProcessedAt IS NOT NULL FROM emails WHERE messageId = 't-skip'",
                    [],
                    |r| r.get(0),
                )
                .or(Ok(false))
            })
            .unwrap();
        assert!(processed, "emails.agentProcessedAt must be stamped on skip");
    }

    /// Successful code-mode draft lands an `actions` row with `mode='code'`
    /// AND `channel='twitter'` (the I10 acceptance criterion). Skips when
    /// `deno` isn't available so the test suite stays portable.
    #[tokio::test]
    async fn code_mode_dry_run_lands_action_row_with_mode_code_and_channel_twitter() {
        if !deno_available_for_tests() {
            eprintln!(
                "skipping code_mode_dry_run_lands_action_row_with_mode_code_and_channel_twitter: \
                 `deno` not on PATH (set AUGMENTAGENT_DENO_BIN to override)"
            );
            return;
        }
        let (store, _f) = tmp_store();
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            // Plain JS body inside a ```ts fence — `extract_ts_block`
            // matches the language tag, while the Deno runner uses indirect
            // eval (no TS-stripping), so the program itself must not carry
            // TypeScript type annotations.
            "```ts\n\
             async function main() {\n\
               await tools.draft(\"twitter\", \"thanks — shipping today\", \"answer the question\");\n\
             }\n\
             main();\n\
             ```\n",
        ]));
        let ch = TwitterChannel::dry_run(store.clone(), reasoner, None);
        let out = ch
            .process_email(tweet_email("t-cm-dry"))
            .await
            .unwrap();
        assert!(matches!(out, Some(DispatchOutcome::DryRun)));

        // mode='code' is the I10 acceptance bit — the dispatcher's
        // terminal `tools.draft` MUST have routed through
        // `log_action_code_mode`. The channel string the dispatcher saw
        // ("twitter") is asserted indirectly: the dispatcher rejects any
        // `tools.draft` whose channel arg disagrees with
        // `MessageContext.channel`, so reaching this point with a
        // successfully landed row already proves channel="twitter" was
        // the live `MessageContext` value (see
        // `code_mode/dispatch.rs:482-486`).
        let actions = store
            .with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT mode, generatedSource, toolCallTrace, draftBody, status \
                     FROM actions WHERE messageId = 't-cm-dry'",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(actions.len(), 1, "expected exactly one actions row");
        let (mode, src, trace, body, status) = &actions[0];
        assert_eq!(mode.as_deref(), Some("code"), "mode must be 'code'");
        assert!(
            src.as_deref()
                .map(|s| s.contains("tools.draft"))
                .unwrap_or(false),
            "generatedSource must contain the program; got {src:?}"
        );
        let trace_str = trace.as_deref().unwrap_or("");
        assert!(
            trace_str.contains("\"call\":\"draft\""),
            "toolCallTrace must include the draft call; got {trace_str:?}"
        );
        assert!(
            trace_str.contains("twitter") || src.as_deref().map(|s| s.contains("twitter")).unwrap_or(false),
            "trace or program must reference 'twitter' channel; trace={trace_str:?} src={src:?}"
        );
        assert_eq!(
            body.as_deref(),
            Some("thanks — shipping today"),
            "draftBody must match what tools.draft passed"
        );
        assert_eq!(status, "dry_run");
    }

    /// **Hard rule: classic Twitter draft path remains reachable.**
    ///
    /// No Deno needed — the code-mode responses lack a fenced block, so
    /// both the initial program emit and the I7 repair retry fail. The
    /// pipeline falls through to the classic prose draft, lands an action
    /// row with `mode='classic'` + `channel='twitter'`, files a single
    /// `code-mode-failure` gh issue (via the mock runner), and posts a
    /// Discord notice tagged with the issue number. This is the I10
    /// analogue of the email channel's
    /// `code_mode_failure_falls_through_to_classic_path` test.
    #[tokio::test]
    async fn code_mode_failure_falls_through_to_classic_twitter_path() {
        let (store, _f) = tmp_store();
        // Triage → reply, then two no-fence code-mode responses (initial +
        // repair retry both fail), then the classic prose-draft call.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"ping"}"#,
            "no fenced block here, just prose",
            "still no fenced block — repair gave up",
            "Yes — shipping today.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let gh = Arc::new(RecordingGh::new(202));
        let ch = TwitterChannel::new(
            store.clone(),
            reasoner,
            broker.clone(),
            TwitterChannelConfig {
                dry_run: false,
                wiki_root: None,
            },
        )
        .with_gh_issue_runner(gh.clone());
        let out = ch.process_email(tweet_email("t-cm-fb")).await.unwrap();
        assert!(matches!(out, Some(DispatchOutcome::AwaitingApproval)));
        // Approval card posted via the classic path.
        assert_eq!(broker.posts.lock().unwrap().len(), 1);

        // Action row landed with mode='classic' (the migration's default
        // for any row inserted via the standard `log_action` path).
        // `channel` is NOT an `actions` column — it travels through the
        // dispatch `MessageContext` and surfaces in the postmortem body
        // (asserted below).
        let mode: Option<String> = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT mode FROM actions WHERE messageId = 't-cm-fb'",
                    [],
                    |r| r.get(0),
                )
                .or(Ok(None))
            })
            .unwrap();
        assert_eq!(
            mode.as_deref(),
            Some("classic"),
            "fallback path must land mode='classic'"
        );

        // I7: exactly one gh issue filed; title prefix, body postmortem
        // marker, channel marker, label all correct.
        let calls = gh.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one gh issue should be filed");
        let (title, body, labels) = &calls[0];
        assert!(title.starts_with("[code-mode]"), "title prefix: {title}");
        assert!(body.contains("## Postmortem"));
        assert!(body.contains("**Final draft mode:** classic"));
        assert!(
            body.contains("**Channel:** twitter"),
            "postmortem must tag channel as twitter; got body={body}"
        );
        assert_eq!(labels, &vec!["code-mode-failure".to_string()]);

        // I7: Discord notice fired with the gh issue number.
        let notices = broker.flag_posts.lock().unwrap();
        assert_eq!(notices.len(), 1, "exactly one Discord notice should fire");
        assert!(notices[0].1.contains("#202"));
        assert!(notices[0].1.contains("classic"));
    }
}
