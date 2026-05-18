//! Shared channel primitives: reasoner trait, prompt builders, decision parsing, wiki ingest,
//! and the [`Trigger`] abstraction for work sources.
//!
//! Platform-specific channels (email, linkedin, slack, …) depend on this crate for the
//! triage → draft → ingest pipeline. Each channel implements its own poll loop and transport,
//! but consumes the same `Reasoner` abstraction and prompt/decision/ingest logic. New platforms
//! additionally implement [`Trigger`] so Phase 2 digests and Phase 3 feed engagement can share
//! one work-source contract instead of inventing their own per platform.

pub mod archetype;
pub mod decision;
pub mod engagement;
pub mod governor;
pub mod ingest;
pub mod prompt;
pub mod reasoner;
pub mod resolve;
pub mod trigger;

pub use decision::{Decision, DecisionKind};
pub use engagement::{
    auto_post_mode_for, AutoPostMode, PostPublisher, PublishOutcome, ScheduledPostEngine,
};
pub use resolve::{
    detect_asks_shadow, default_resolvers, AskResolveMode, AskResolver, DetectedAsk,
    ResolvedFill, ResolverKind,
};
pub use governor::{
    lookup_limit, next_action_delay, quiet_hours_until, requires_approval, scale_cap,
    warmup_curve, ActionKind, ActionRequest, Clock, Denial, HaltReason, HaltState, Outcome,
    Permit, Platform, RateCaps, RateGovernor, RateLimit, Risk, SqliteGovernor, SystemClock,
    TargetAttrs, WindowedCounter, RATE_TABLE,
};
pub use reasoner::{ClaudeCliReasoner, Reasoner, ReasonerOpts};
pub use trigger::{
    kind as work_item_kind, ChannelRunner, DigestSource, FriendFeedSource, InboundMessageTrigger,
    InboundSource, Trigger, WorkItem, WorkItemHandler,
};
