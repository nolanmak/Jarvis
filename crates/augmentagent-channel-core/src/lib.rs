//! Shared channel primitives: reasoner trait, prompt builders, decision parsing, wiki ingest,
//! and the [`Trigger`] abstraction for work sources.
//!
//! Platform-specific channels (email, linkedin, slack, …) depend on this crate for the
//! triage → draft → ingest pipeline. Each channel implements its own poll loop and transport,
//! but consumes the same `Reasoner` abstraction and prompt/decision/ingest logic. New platforms
//! additionally implement [`Trigger`] so Phase 2 digests and Phase 3 feed engagement can share
//! one work-source contract instead of inventing their own per platform.

pub mod archetype;
pub mod cerebras;
pub mod code_mode;
pub mod codex;
pub mod cooldown;
pub mod decision;
pub mod engagement;
pub mod fallback;
pub mod gemini;
pub mod governor;
pub mod images;
pub mod ingest;
pub mod mcp;
pub mod memory_nudge;
pub mod prompt;
pub mod providers;
pub mod reasoner;
pub mod resolve;
pub mod secret_loader;
pub mod skills;
pub mod timeparse;
pub mod tool_audit;
pub mod trigger;

pub use decision::{Decision, DecisionKind};
pub use engagement::{
    auto_post_mode_for, AutoPostMode, PostPublisher, PublishOutcome, ScheduledPostEngine,
};
pub use resolve::{
    default_resolvers, detect_asks_shadow, live_resolvers, resolve_asks, resolve_asks_block,
    resolved_asks_block, AskResolveMode, AskResolver, BusyInterval, ComposioResolveClient,
    DetectedAsk, DriveHit, DriveSearchApi, FreeBusyApi, ResolveCtx, ResolveOutcome,
    ResolvedFill, ResolverKind, UnresolvedAsk,
};
pub use governor::{
    lookup_limit, next_action_delay, quiet_hours_until, requires_approval, scale_cap,
    warmup_curve, ActionKind, ActionRequest, Clock, Denial, HaltReason, HaltState, Outcome,
    Permit, Platform, RateCaps, RateGovernor, RateLimit, Risk, SqliteGovernor, SystemClock,
    TargetAttrs, WindowedCounter, RATE_TABLE,
};
pub use mcp::{default_mcp_config_path, McpConfig, McpServerConfig};
pub use memory_nudge::{default_cycles_root, CycleLogger, CycleSummary, CycleSurface};
pub use reasoner::{ClaudeCliReasoner, Reasoner, ReasonerError, ReasonerOpts};
// #655 — multi-provider failover: one Reasoner seam, N provider adapters.
pub use cooldown::CooldownLatch;
pub use fallback::{build_reasoner, FallbackReasoner};
pub use images::{extract_image_markers, image_marker_line, IMAGE_MARKER_PREFIX};
pub use providers::{CapabilityClass, ModelTier, ProviderKind};
pub use skills::{SkillEntry, SkillRegistry};
// #501 — deterministic send-time parsing for scheduled sends (shared with
// the query-mode `--send-at` flag, #502).
pub use timeparse::{
    parse_send_at, resolve_token, validate_send_at, MAX_HORIZON_MS, MIN_LEAD_MS, SKEW_MS,
};
pub use tool_audit::{
    build_audit_record, default_audit_log_path, format_notice, is_high_risk, parse_stream_event,
    truncate_stream, AuditLogger, AuditNotifier, AuditRecord, ToolEvent,
};
pub use trigger::{
    kind as work_item_kind, ChannelRunner, DigestSource, FriendFeedSource, FriendFeedTrigger,
    InboundMessageTrigger, InboundSource, Trigger, WorkItem, WorkItemHandler,
};
