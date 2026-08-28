//! Provider registry for the multi-provider reasoner chain (#655/#658).
//!
//! Presets stay exactly as they are today — Claude-shaped `ReasonerOpts`
//! with a pinned Claude model id. This module derives everything the
//! fallback layer needs FROM those opts (no dual paths, no preset rewrites):
//!
//! - [`ModelTier`] from the pinned Claude model (haiku ⇒ Fast, else Quality),
//!   then a per-provider tier→model map (env-overridable per cell) picks the
//!   concrete model a fallback provider runs.
//! - [`CapabilityClass`] from the preset's declared surface (tools, hooks,
//!   MCP, restrict_env), which gates WHICH providers a call may fall over to.
//!
//! ## Fallback eligibility policy (epic #655 matrix)
//!
//! | class        | claude | codex | gemini | cerebras |
//! |--------------|--------|-------|--------|----------|
//! | text-only    |   ✓    |   ✓   |   ✓    |    ✓     |
//! | read-tools   |   ✓    |  ✗(²) |   ✓    |    ✗ (¹) |
//! | write-tools  |   ✓    |   ✗   |   ✗    |    ✗ (²) |
//! | full-agentic |   ✓    |   ✗   |   ✗    |    ✗ (²) |
//!
//! (¹) Cerebras read-tools is gated on the offline triage eval (#665).
//! (²) Gated on the security-parity probes (#664): codex's sandbox does not
//!     path-scope READS (whole-disk read + always-on shell — see
//!     [`allowed_for`]), and the guard hooks that enforce wiki path scoping
//!     are Claude-specific today with documented fail-open traps on both
//!     codex and gemini.

use crate::reasoner::ReasonerOpts;

/// A provider in the fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Claude,
    Codex,
    Gemini,
    /// Cerebras rides the codex CLI via a custom `model_provider` block
    /// (base_url + wire_api="chat") — same spawn/parse path as Codex, no
    /// second HTTP client (#663).
    Cerebras,
}

impl ProviderKind {
    pub fn name(self) -> &'static str {
        match self {
            ProviderKind::Claude => "claude",
            ProviderKind::Codex => "codex",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Cerebras => "cerebras",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(ProviderKind::Claude),
            "codex" => Some(ProviderKind::Codex),
            "gemini" | "google" => Some(ProviderKind::Gemini),
            "cerebras" => Some(ProviderKind::Cerebras),
            _ => None,
        }
    }
}

/// Quality band a preset asked for. Presets keep pinning Claude model ids
/// (the #448 no-inheritance invariant); the tier is derived, not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    /// Opus-class: triage, drafts, digest, ask.
    Quality,
    /// Haiku-class: parsers, extractors, classifiers.
    Fast,
}

/// Derive the tier from the preset's pinned Claude model. `None` should not
/// happen for production presets (#448 pins them all) — treat it as Quality
/// so an unpinned call can never silently land on a cheap fallback model.
pub fn tier_of(opts: &ReasonerOpts) -> ModelTier {
    match &opts.model {
        Some(m) if m.to_ascii_lowercase().contains("haiku") => ModelTier::Fast,
        _ => ModelTier::Quality,
    }
}

/// What surface the preset declared. Derived from the opts a preset already
/// carries, so adding a field to every preset (and every test literal in the
/// workspace) is unnecessary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityClass {
    /// No tools at all — pure text transform.
    TextOnly,
    /// Read-only file tools (Read/Grep/Glob on the wiki).
    ReadTools,
    /// Write/Edit on the wiki.
    WriteTools,
    /// Bash allowlists, MCP servers, guard hooks, restrict_env — the
    /// wiki-ask / auto-PR-build shape.
    FullAgentic,
}

pub fn classify(opts: &ReasonerOpts) -> CapabilityClass {
    let agentic = opts.settings_json.is_some()
        || opts.restrict_env
        || opts
            .allowed_tools
            .iter()
            .any(|t| t.starts_with("Bash(") || t.starts_with("mcp__"));
    if agentic {
        return CapabilityClass::FullAgentic;
    }
    if opts
        .allowed_tools
        .iter()
        .any(|t| t == "Write" || t == "Edit" || t == "NotebookEdit")
    {
        return CapabilityClass::WriteTools;
    }
    // FAIL-CLOSED (#655 review): only tools we positively know are read-only
    // may classify as ReadTools. A bare "Bash", "Task", "WebFetch", or any
    // future tool name lands in FullAgentic (claude-only) rather than
    // silently widening a fallback provider's surface.
    if !opts.allowed_tools.is_empty() {
        let known_read_only =
            |t: &String| matches!(t.as_str(), "Read" | "Grep" | "Glob" | "LS" | "WebSearch");
        if opts.allowed_tools.iter().all(known_read_only) {
            return CapabilityClass::ReadTools;
        }
        return CapabilityClass::FullAgentic;
    }
    CapabilityClass::TextOnly
}

/// May `kind` serve a call of `class`? See the module-level policy table.
///
/// Codex is TEXT-ONLY for now (#655 review / #664): its read-only sandbox
/// confines writes and network but NOT reads — the model's shell can read
/// any file on disk (`.env`, `~/.claude/.credentials.json`, `~/.ssh`), and
/// read-tools presets feed it untrusted email content. Until the #664
/// managed PreToolUse deny-hook ships and the escape probes pass, only
/// no-tool text transforms may route there (their outputs are human-gated
/// drafts / narrow parsed fields). Gemini KEEPS read-tools: its file tools
/// are workspace-confined to the pinned cwd and our per-spawn settings strip
/// the shell tool entirely (`tools.core` allowlist).
pub fn allowed_for(kind: ProviderKind, class: CapabilityClass) -> bool {
    match kind {
        ProviderKind::Claude => true,
        ProviderKind::Gemini => matches!(
            class,
            CapabilityClass::TextOnly | CapabilityClass::ReadTools
        ),
        ProviderKind::Codex => {
            matches!(class, CapabilityClass::TextOnly)
                || (codex_read_tools_enabled() && matches!(class, CapabilityClass::ReadTools))
        }
        ProviderKind::Cerebras => matches!(class, CapabilityClass::TextOnly),
    }
}

/// #828 — opt-in widening of codex to READ-TOOLS presets.
///
/// The blanket text-only rule above is written against the *shell*: "the
/// model's shell can read any file on disk (`.env`,
/// `~/.claude/.credentials.json`, `~/.ssh`)". `CapabilityClass::ReadTools`
/// structurally excludes that — `classify` only returns it when EVERY
/// allowed tool is `Read`/`Grep`/`Glob`/`LS`/`WebSearch`, so a preset in this
/// class has no `Bash` and therefore no shell at all. The rationale for the
/// ban does not reach this class.
///
/// It is still a widening, so it is off by default and opted into per host:
/// codex's file tools are not proven cwd-confined the way gemini's are, so a
/// prompt-injected run could still `Read` an absolute path outside the
/// worktree. What bounds the damage on the auto-PR review path that motivated
/// this:
///
/// - **Inputs are inside the trust boundary.** The reviewer sees an
///   owner-authored issue (#300 gate) and a diff Claude wrote — not the
///   untrusted inbound email that the original text-only rule was defending
///   against.
/// - **No secrets in reach of the obvious paths.** The review runs in a fresh
///   `git worktree`, which carries no `.env` (gitignored) and no credentials;
///   the codex adapter already spawns with `env_clear()` plus an allowlist.
/// - **No network egress from tool calls** — codex's Landlock sandbox blocks
///   it, so the only output channel is the review text itself, which lands on
///   the owner's own private PR.
///
/// Kernel-level read confinement (Landlock on the spawn, or `bwrap` once
/// unprivileged user namespaces are permitted — AppArmor blocks them on this
/// host today) is the real fix and is tracked separately. Until then this
/// flag is the explicit, auditable place where the trade-off is made.
pub fn codex_read_tools_enabled() -> bool {
    matches!(
        std::env::var("AUGMENTAGENT_CODEX_READ_TOOLS").ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// Default tier→model map per provider. Every cell is overridable without a
/// rebuild via `AUGMENTAGENT_MODEL_<PROVIDER>_<TIER>` (e.g.
/// `AUGMENTAGENT_MODEL_CODEX_QUALITY=gpt-5.6-sol`) — the "swap models in and
/// out" requirement. Defaults chosen 2026-08-19; the doctor check (#667)
/// flags pinned ids that stop existing (Cerebras deprecated five model
/// families in twelve months).
pub fn model_for(kind: ProviderKind, tier: ModelTier) -> String {
    let tier_name = match tier {
        ModelTier::Quality => "QUALITY",
        ModelTier::Fast => "FAST",
    };
    let env_key = format!(
        "AUGMENTAGENT_MODEL_{}_{}",
        kind.name().to_ascii_uppercase(),
        tier_name
    );
    if let Ok(v) = std::env::var(&env_key) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    match (kind, tier) {
        // Claude cells are unused in practice (presets pin their own model,
        // which the Claude adapter passes through) but kept total so the
        // map has no panicking holes.
        (ProviderKind::Claude, ModelTier::Quality) => "claude-opus-4-8".into(),
        (ProviderKind::Claude, ModelTier::Fast) => "claude-haiku-4-5-20251001".into(),
        (ProviderKind::Codex, ModelTier::Quality) => "gpt-5.6-terra".into(),
        (ProviderKind::Codex, ModelTier::Fast) => "gpt-5.6-luna".into(),
        (ProviderKind::Gemini, ModelTier::Quality) => "gemini-3.1-pro-preview".into(),
        (ProviderKind::Gemini, ModelTier::Fast) => "gemini-3.7-flash".into(),
        (ProviderKind::Cerebras, ModelTier::Quality) => "gpt-oss-120b".into(),
        (ProviderKind::Cerebras, ModelTier::Fast) => "gemma-4-31b".into(),
    }
}

/// Parse the configured chain. Default is `claude` alone — failover is
/// opt-in via `.env` so a bare checkout behaves exactly as before #655.
/// Unknown entries are logged-and-skipped rather than fatal so a typo can't
/// take the reasoner down; duplicates are dropped.
pub fn chain_from_env() -> Vec<ProviderKind> {
    let raw = std::env::var("AUGMENTAGENT_REASONER_CHAIN").unwrap_or_default();
    let mut chain: Vec<ProviderKind> = Vec::new();
    for tok in raw.split(',') {
        if tok.trim().is_empty() {
            continue;
        }
        match ProviderKind::parse(tok) {
            Some(k) if !chain.contains(&k) => chain.push(k),
            Some(_) => {}
            None => tracing::warn!("AUGMENTAGENT_REASONER_CHAIN: unknown provider {tok:?} skipped"),
        }
    }
    if chain.is_empty() {
        chain.push(ProviderKind::Claude);
    }
    chain
}

/// Does `bin` resolve to an executable? Absolute/relative paths are checked
/// directly; bare names are searched on PATH. Used by the eligibility check
/// so an uninstalled fallback CLI is skipped at chain-build time with one
/// log line instead of failing every call.
pub fn bin_resolves(bin: &str) -> bool {
    let p = std::path::Path::new(bin);
    if p.components().count() > 1 {
        return p.is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file())
}

#[cfg(test)]
mod tests {
    // ---- #828: codex read-tools is opt-in, never on by accident ----

    #[test]
    fn codex_read_tools_is_off_unless_explicitly_enabled() {
        let _g = crate::providers::tests::env_guard();
        let prev = std::env::var("AUGMENTAGENT_CODEX_READ_TOOLS").ok();
        std::env::remove_var("AUGMENTAGENT_CODEX_READ_TOOLS");
        assert!(
            !allowed_for(ProviderKind::Codex, CapabilityClass::ReadTools),
            "the widening must be opt-in; a bare checkout keeps codex text-only"
        );
        assert!(allowed_for(ProviderKind::Codex, CapabilityClass::TextOnly));

        std::env::set_var("AUGMENTAGENT_CODEX_READ_TOOLS", "1");
        assert!(allowed_for(ProviderKind::Codex, CapabilityClass::ReadTools));
        // Never beyond read-tools: FullAgentic keeps the shell, which is what
        // the text-only rule was actually written against.
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::FullAgentic));
        // And it must not leak to another provider.
        assert!(!allowed_for(ProviderKind::Cerebras, CapabilityClass::ReadTools));

        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_CODEX_READ_TOOLS", v),
            None => std::env::remove_var("AUGMENTAGENT_CODEX_READ_TOOLS"),
        }
    }

    pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    use super::*;

    fn opts(tools: Vec<&str>, model: Option<&str>) -> ReasonerOpts {
        ReasonerOpts {
            system_prompt: "s".into(),
            model: model.map(str::to_string),
            allowed_tools: tools.into_iter().map(str::to_string).collect(),
            add_dirs: vec![],
            permission_mode: "default".into(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
            audit_logger: None,
            audit_notifier: None,
            session_id: None,
        }
    }

    #[test]
    fn classify_matches_the_preset_matrix() {
        // The real presets, by shape:
        assert_eq!(classify(&opts(vec![], None)), CapabilityClass::TextOnly); // tone/loop/archetype
        assert_eq!(
            classify(&opts(vec!["Read", "Grep", "Glob"], None)),
            CapabilityClass::ReadTools
        ); // triage/draft/digest/lint
        assert_eq!(
            classify(&opts(vec!["Read", "Grep", "Glob", "Write", "Edit"], None)),
            CapabilityClass::WriteTools
        ); // ingest/resume
        assert_eq!(
            classify(&opts(vec!["Read", "Bash(augmentagent gmail *)"], None)),
            CapabilityClass::FullAgentic
        ); // ask-shaped
        let mut with_settings = opts(vec!["Read"], None);
        with_settings.settings_json = Some("{}".into());
        assert_eq!(classify(&with_settings), CapabilityClass::FullAgentic);
        let mut restricted = opts(vec![], None);
        restricted.restrict_env = true;
        assert_eq!(classify(&restricted), CapabilityClass::FullAgentic);
        let mcp = opts(vec!["mcp__memory__memory_search"], None);
        assert_eq!(classify(&mcp), CapabilityClass::FullAgentic);
    }

    #[test]
    fn tier_derivation_and_policy() {
        assert_eq!(
            tier_of(&opts(vec![], Some("claude-haiku-4-5-20251001"))),
            ModelTier::Fast
        );
        assert_eq!(tier_of(&opts(vec![], Some("claude-opus-4-8"))), ModelTier::Quality);
        // Unpinned never lands on a cheap fallback model.
        assert_eq!(tier_of(&opts(vec![], None)), ModelTier::Quality);

        assert!(allowed_for(ProviderKind::Claude, CapabilityClass::FullAgentic));
        // Codex is text-only by default. #828 lets a host opt into ReadTools
        // via AUGMENTAGENT_CODEX_READ_TOOLS, so this must PIN the flag rather
        // than read whatever the ambient environment happens to carry — on the
        // production host it is set in `.env`, and the gate inherits it, which
        // turned this into a red suite for every auto-PR run.
        let _g = env_guard();
        let prev = std::env::var("AUGMENTAGENT_CODEX_READ_TOOLS").ok();
        std::env::remove_var("AUGMENTAGENT_CODEX_READ_TOOLS");
        assert!(allowed_for(ProviderKind::Codex, CapabilityClass::TextOnly));
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::ReadTools));
        // Never beyond read-tools, opted in or not — the shell is what the
        // text-only rule was written against.
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::WriteTools));
        std::env::set_var("AUGMENTAGENT_CODEX_READ_TOOLS", "1");
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::WriteTools));
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::FullAgentic));
        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_CODEX_READ_TOOLS", v),
            None => std::env::remove_var("AUGMENTAGENT_CODEX_READ_TOOLS"),
        }
        assert!(allowed_for(ProviderKind::Gemini, CapabilityClass::ReadTools));
        assert!(!allowed_for(ProviderKind::Gemini, CapabilityClass::FullAgentic));
        assert!(allowed_for(ProviderKind::Cerebras, CapabilityClass::TextOnly));
        assert!(!allowed_for(ProviderKind::Cerebras, CapabilityClass::ReadTools));
    }

    /// #448's invariant extended to every provider (#658): the model each
    /// fallback provider runs is explicit — a filled map cell or an explicit
    /// env override — never an inherited interactive default.
    #[test]
    fn no_fallback_preset_inherits_an_interactive_model() {
        for kind in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Gemini,
            ProviderKind::Cerebras,
        ] {
            for tier in [ModelTier::Quality, ModelTier::Fast] {
                let m = model_for(kind, tier);
                assert!(
                    !m.trim().is_empty() && m != "auto",
                    "{}/{tier:?} resolved to a floating model: {m:?}",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn chain_env_parses_and_defaults() {
        // NB: env-var tests share process env; use a distinct var lifecycle.
        std::env::set_var("AUGMENTAGENT_REASONER_CHAIN", "claude, codex,nope,gemini,claude");
        let chain = chain_from_env();
        std::env::remove_var("AUGMENTAGENT_REASONER_CHAIN");
        assert_eq!(
            chain,
            vec![ProviderKind::Claude, ProviderKind::Codex, ProviderKind::Gemini]
        );

        std::env::remove_var("AUGMENTAGENT_REASONER_CHAIN");
        assert_eq!(chain_from_env(), vec![ProviderKind::Claude]);
    }
}
