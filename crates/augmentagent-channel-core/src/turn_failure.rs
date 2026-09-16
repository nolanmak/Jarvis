//! Why a provider turn failed, and what the fallback chain may do about it
//! (#1040).
//!
//! When a CLI adapter sees a failed or empty turn, the question is whether
//! the provider failed or the request did. If the provider failed, another
//! provider might succeed now, and this one should rest. If the request
//! failed, every provider would fail it the same way, and some of its work
//! may already be done. Mistaking a request failure for a provider failure
//! latches a healthy provider and gives a half-finished write to the next
//! provider, which then does it again. So classification is one ordered
//! table, and anything the table does not recognise lands on the safe side.
//!
//! | class          | surfaced as                    | latch             | chain advance |
//! |----------------|--------------------------------|-------------------|---------------|
//! | `Readiness`    | `ReasonerError::Local`         | never             | yes           |
//! | `Quota`        | `ReasonerError::RateLimited`   | until the reset   | yes           |
//! | `Transport`    | `ReasonerError::Unavailable`   | short             | yes           |
//! | `Auth`         | `ReasonerError::Unavailable`   | short             | yes           |
//! | `Binary`       | `ReasonerError::Unavailable`   | short             | yes           |
//! | `Content`      | [`TurnFailure`]                | never             | never         |
//! | `Unrecognised`, text or read call | `ReasonerError::Unavailable` | short | yes |
//! | `Unrecognised`, write or agentic call | [`TurnFailure`] | short, after repeated strikes | never |
//!
//! [`TurnFailure`] is deliberately not a [`ReasonerError`]. `FallbackReasoner`
//! returns any such error immediately. Claude's `EmptyOutput` is a Content
//! `TurnFailure` too. Callers can still downcast it to read the class. When a
//! write or agentic call ends in Content, the chain also checks the request's
//! operation journal (see `handoff_outcome`), so a caller retry cannot repeat
//! work that already finished.
//!
//! **Default when nothing matches: `Unrecognised`, routed by what the call
//! can do.**
//! - A write or agentic call fails safe: the chain does not advance, and the
//!   first failures latch nothing. A content failure misread as an outage
//!   would latch a healthy provider (for hours, on a quota misread) and
//!   dispatch the request again, repeating effects that may already have
//!   happened. Some unknown failures are real outages, though, and they must
//!   not respawn the provider on every later request. So `FallbackReasoner`
//!   latches the provider for the short outage cooldown after a few
//!   consecutive unrecognised failures.
//! - A text-only or read-only call cannot repeat a write, so an unrecognised
//!   failure there is an outage: latch and fail over. Otherwise a provider
//!   failing in a way the table does not know would be respawned on every
//!   triage re-poll, and the next provider would never serve.
//!
//! Content rows are matched before the other needle rows for the same reason.
//! An explicit HTTP status (`status 429`, `status 503`, `status 401`) is
//! matched before any needle. It is the transport's own verdict, while the
//! body is free text that can quote anything. If the provider exits before
//! reporting any turn or item event, no tool can have run, so an unexplained
//! exit counts as the binary failing to start and fails over, as it did
//! before #1040.

use crate::providers::CapabilityClass;
use crate::reasoner::{parse_reset_hint, ReasonerError};

/// One failure class per row of the module table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// A required Jarvis bridge/MCP server was not ready. Our side.
    Readiness,
    /// Quota, plan or billing wall.
    Quota,
    /// Network, stream, gateway, overload or 5xx failure between CLI and API.
    Transport,
    /// Login, token or credential failure.
    Auth,
    /// The CLI process itself failed: crashed, or exited before any turn.
    Binary,
    /// The turn failed on the request itself: context overflow, empty output,
    /// a policy refusal, the bridge refusing further tool calls.
    Content,
    /// Nothing in the table matched after the turn began. Fail safe.
    Unrecognised,
}

impl FailureClass {
    /// Whether the fallback chain may latch the provider and try the next
    /// one. Only transport, auth, binary and quota failures do.
    pub fn is_provider_side(self) -> bool {
        matches!(self, Self::Quota | Self::Transport | Self::Auth | Self::Binary)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Readiness => "local readiness failure",
            Self::Quota => "quota",
            Self::Transport => "transport failure",
            Self::Auth => "authentication failure",
            Self::Binary => "binary failure",
            Self::Content => "content-level failure",
            Self::Unrecognised => "unrecognised failure",
        }
    }
}

/// Marker the bridge prefixes to its fixed readiness categories.
const READINESS_MARKER: &str = "JARVIS_READINESS:";

/// Readiness categories and the fixed text returned for them. The native
/// failure text can carry private configuration, so it is never repeated.
const READINESS: &[(&str, &str)] = &[
    ("mcp_start", "required MCP server could not start or initialize; check its binary, authentication and transport"),
    ("mcp_timeout", "required MCP server timed out; check its availability and timeout setting"),
    ("mcp_tools", "required MCP tool is missing; check the server version and tool profile"),
];

/// The needle rows, in match order: the first row with a needle found in the
/// lowercased failure text wins. `Content` comes first (see the module doc).
/// Needles are the wording codex and the Responses API use, taken from the
/// installed CLI (`strings`) where it has one. Keep them specific: a needle
/// that shows up inside unrelated text becomes a latch.
const RULES: &[(FailureClass, &[&str])] = &[
    (FailureClass::Content, &[
        // Context overflow, in each wording codex and the API use.
        "context window",
        "context_length_exceeded",
        "maximum context length",
        "exceeds the model-context limit",
        "input exceeds the maximum length",
        "ran out of room",
        // A policy refusal of this request. Not a bare "usage policy" or
        // "flagged for possible": quota and account messages use those too.
        "invalid prompt:",
        "invalid_prompt",
        "violating our usage policy",
        "content_filter",
        "misalignment policy",
        "cyber policy",
        "request has been flagged for possible",
        "content was flagged for possible",
        // The Jarvis bridge refusing further tool calls in this request.
        "requires reconciliation",
        "reconciliation required",
        "already completed for this request",
    ]),
    (FailureClass::Quota, &[
        "usage limit",
        "rate limit",
        "rate_limit",
        "too many requests",
        "insufficient_quota",
        "resource_exhausted",
        "payment required",
        "quota exceeded",
        "quota exhausted",
        "exceeded your current quota",
        "check your plan and billing",
        "usage not included",
        "usage_not_included",
        // Plan and workspace walls.
        "upgrade to plus",
        "out of credits",
        "spend cap",
    ]),
    (FailureClass::Auth, &[
        "unauthorized",
        "forbidden",
        "not logged in",
        "invalid api key",
        "incorrect api key",
        "invalid_api_key",
        "access token",
        "refresh token",
        "authentication token",
        "authentication session",
        "token expired",
        "token has expired",
        "token is expired",
        "token_expired",
        "token data",
        "auth is missing",
        "auth is not configured",
        "sign in again",
        "signing in again",
        "log in again",
        "deactivated",
    ]),
    (FailureClass::Transport, &[
        "stream disconnected",
        "connection failed",
        "connection refused",
        "connection reset",
        "connection closed",
        "error sending request",
        "error while reading the server response",
        "exceeded retry limit",
        "timed out",
        "dns error",
        "failed to lookup address",
        "network is unreachable",
        "no route to host",
        "upstream connect error",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "temporarily unavailable",
        "overloaded",
        "at capacity",
        "high demand",
        "access blocked by cloudflare",
    ]),
    (FailureClass::Binary, &[
        "panicked at",
        "agent loop died",
    ]),
];

/// HTTP statuses that name a quota wall on their own. Matched as standalone
/// digit tokens only (#655 review): a bare substring fired inside request ids.
const QUOTA_STATUS_CODES: &[&str] = &["429", "402"];

/// The class an explicit HTTP status names: `unexpected status 520 …`,
/// `last status: 529`, `HTTP/1.1 503`. 400, 403 and 404 name no class on
/// their own (a 400 can be a context overflow, a 403 a policy block, a 404 a
/// mistyped model override), so their bodies decide.
fn explicit_status(lower: &str) -> Option<FailureClass> {
    static STATUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(?:status|http)(?:/\d(?:\.\d)?)?[\s:=]*(\d{3})\b").expect("status pattern")
    });
    STATUS.captures_iter(lower).find_map(|captures| match captures[1].parse::<u16>().ok()? {
        402 | 429 => Some(FailureClass::Quota),
        401 => Some(FailureClass::Auth),
        500..=599 => Some(FailureClass::Transport),
        _ => None,
    })
}

/// Classify a provider's failure text. Set `turn_began` once the provider
/// has reported any turn or item event, because tools may have run from then
/// on.
pub fn classify(detail: &str, turn_began: bool) -> FailureClass {
    if detail.contains(READINESS_MARKER) {
        return FailureClass::Readiness;
    }
    let lower = detail.to_ascii_lowercase();
    if let Some(class) = explicit_status(&lower) {
        return class;
    }
    let status_token = |code: &&str| lower.split(|c: char| !c.is_ascii_digit()).any(|t| t == *code);
    for (class, needles) in RULES {
        if needles.iter().any(|needle| lower.contains(needle))
            || (*class == FailureClass::Quota && QUOTA_STATUS_CODES.iter().any(status_token))
        {
            return *class;
        }
    }
    if turn_began { FailureClass::Unrecognised } else { FailureClass::Binary }
}

/// A turn that ended content-level, or for a reason nothing recognised. Not a
/// [`ReasonerError`] on purpose: the fallback chain neither latches the
/// provider nor tries the next one.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TurnFailure {
    pub provider: String,
    pub class: FailureClass,
    message: String,
}

impl TurnFailure {
    /// The turn finished without final assistant text. Same wording as
    /// claude's `EmptyOutput`.
    pub(crate) fn empty_output(provider: &str) -> Self {
        Self {
            provider: provider.into(),
            class: FailureClass::Content,
            message: format!("{provider} produced no assistant text"),
        }
    }
}

/// The error the fallback chain acts on for `class` on a call of
/// `capability`, following the module table.
pub(crate) fn turn_error(provider: &str, class: FailureClass, capability: CapabilityClass, detail: String) -> anyhow::Error {
    let short = || detail.chars().take(300).collect::<String>();
    match class {
        FailureClass::Readiness => {
            let message = READINESS.iter()
                .find(|(category, _)| detail.contains(&format!("{READINESS_MARKER}{category} ")))
                .map(|(_, message)| *message)
                .unwrap_or("required MCP server is not ready; check its configuration");
            ReasonerError::Local { message: format!("{provider}: {message}") }.into()
        }
        FailureClass::Quota => ReasonerError::RateLimited {
            provider: provider.into(),
            reset_at: parse_reset_hint(&detail),
            message: detail,
        }.into(),
        FailureClass::Transport | FailureClass::Auth | FailureClass::Binary => {
            ReasonerError::Unavailable { provider: provider.into(), message: short() }.into()
        }
        // A text or read call cannot repeat a write, so an unexplained failure
        // there is an outage: the chain latches the provider and moves on.
        FailureClass::Unrecognised if matches!(capability, CapabilityClass::TextOnly | CapabilityClass::ReadTools) => {
            ReasonerError::Unavailable {
                provider: provider.into(),
                message: format!("{}: {}", class.label(), short()),
            }.into()
        }
        FailureClass::Content | FailureClass::Unrecognised => TurnFailure {
            provider: provider.into(),
            class,
            message: format!("{provider} turn failed ({}): {}", class.label(), short()),
        }.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FailureClass::*;

    /// The table, pinned row by row with the real wording each row exists
    /// for. A new failure shape gets a line here before a needle.
    #[test]
    fn classifier_table_pins_real_failure_wording() {
        let cases: &[(&str, FailureClass)] = &[
            ("Codex ran out of room in the model's context window. Start a new thread or clear earlier history before retrying.", Content),
            ("Your input exceeds the context window of this model. Please adjust your input and try again.", Content),
            ("unexpected status 400 Bad Request: {\"error\":{\"code\":\"context_length_exceeded\"}}", Content),
            ("This model's maximum context length is 400000 tokens.", Content),
            ("Input exceeds the maximum length of 1048576 characters", Content),
            ("Invalid prompt: your prompt was flagged as potentially violating our usage policy.", Content),
            ("This request was blocked due to a misalignment policy violation.", Content),
            ("This request has been flagged for possible cybersecurity risk.", Content),
            ("uncertain operation requires reconciliation before further mutations", Content),
            ("External operation already completed for this request; use its prior result instead of repeating it.", Content),
            ("You've hit your usage limit. Try again at Aug 20th, 2026 10:27 AM.", Quota),
            ("You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro)", Quota),
            ("Quota exceeded. Check your plan and billing details.", Quota),
            ("exceeded retry limit, last status: 429 Too Many Requests", Quota),
            ("{\"error\":{\"code\":\"insufficient_quota\"}}", Quota),
            ("429 RESOURCE_EXHAUSTED", Quota),
            ("402 Payment Required", Quota),
            ("usage_not_included", Quota),
            ("unexpected status 401 Unauthorized: missing bearer", Auth),
            ("unexpected status 403 Forbidden", Auth),
            ("Not logged in", Auth),
            ("Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.", Auth),
            ("stream disconnected before completion: error sending request", Transport),
            ("Connection failed: error sending request for url", Transport),
            ("Error while reading the server response: connection reset", Transport),
            ("exceeded retry limit, last status: 503 Service Unavailable", Transport),
            ("unexpected status 500 Internal Server Error", Transport),
            ("Selected model is at capacity. Please try a different model.", Transport),
            ("We're currently experiencing high demand, which may cause temporary errors.", Transport),
            ("request timed out", Transport),
            ("thread 'main' panicked at core/src/synthetic.rs:1:1:", Binary),
            ("required MCP server: JARVIS_READINESS:mcp_start PRIVATE_SYNTHETIC", Readiness),
        ];
        for (detail, want) in cases {
            assert_eq!(classify(detail, true), *want, "{detail}");
        }
    }

    /// #1069 review H2/L3: exact wording from the installed codex 0.154
    /// binary (`strings`), plus the probe's HTTP shapes, that the first
    /// table missed or misread. Each line is a string codex can emit.
    #[test]
    fn codex_0_154_plan_auth_capacity_and_status_wording_is_classified() {
        let cases: &[(&str, FailureClass)] = &[
            // Plan and billing walls.
            ("To use Codex with your ChatGPT plan, upgrade to Plus: https://chatgpt.com/explore/plus.", Quota),
            ("Your workspace is out of credits. Ask your workspace owner to refill in order to continue.", Quota),
            ("You hit your spend cap set in your workspace. Increase your spend cap to continue.", Quota),
            ("You hit your spend cap set by the owner of your workspace. Ask an owner to increase your spend cap to continue.", Quota),
            ("You've hit your usage limit for gpt-5.5-codex. Switch to another model now, or try again at 3:00 PM.", Quota),
            ("You exceeded your current quota, please check your plan and billing details.", Quota),
            ("stream disconnected before completion: Your organization has exceeded its usage limit; see usage policy", Quota),
            // An explicit HTTP status outranks free text in its body.
            ("unexpected status 429 Too Many Requests: usage policy note: context window headroom", Quota),
            // Auth.
            ("Token data is not available.", Auth),
            ("ChatGPT auth is missing token data", Auth),
            ("external auth is not configured", Auth),
            ("Your authentication session could not be refreshed automatically. Please log out and sign in again.", Auth),
            ("Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again.", Auth),
            ("unexpected status 401: Provided authentication token is expired. Please try signing in again., auth error code: token_expired", Auth),
            ("unexpected status 403 Forbidden: Your account was flagged for possible abuse and has been deactivated", Auth),
            ("Your workspace has been deactivated", Auth),
            // Transport and capacity, including 5xx without a standard phrase.
            ("unexpected status 520 <html>cloudflare</html>", Transport),
            ("unexpected status 503: upstream connect error or disconnect/reset before headers. reset reason: overflow", Transport),
            ("exceeded retry limit, last status: 529, request id: abc", Transport),
            ("Server overloaded; retry later.", Transport),
            // The CLI's own agent loop crashing.
            ("internal error; agent loop died unexpectedly", Binary),
            // Content refusals, in their exact wording.
            ("Invalid prompt: we've limited access to this content for safety reasons.", Content),
            ("This content was flagged for possible biological risk.", Content),
            ("cyber policy: synthetic refusal", Content),
        ];
        for (detail, want) in cases {
            assert_eq!(classify(detail, true), *want, "{detail}");
        }
    }

    /// Codex configuration faults (a bad `AUGMENTAGENT_MODEL_CODEX_*`
    /// override) stay unrecognised: the capability class, not the table,
    /// decides whether they fail over (see `routes_unrecognised_by_capability`).
    #[test]
    fn model_configuration_faults_stay_unrecognised() {
        for detail in [
            "unexpected status 400 Bad Request: {\"detail\":\"The 'gpt-9' model is not supported when using Codex with a ChatGPT account.\"}",
            "unexpected status 404 Not Found: {\"error\":{\"message\":\"The model `gpt-9` does not exist or you do not have access to it.\",\"code\":\"model_not_found\"}}",
        ] {
            assert_eq!(classify(detail, true), Unrecognised, "{detail}");
        }
    }

    #[test]
    fn content_is_matched_before_any_provider_row() {
        // A 400 carrying a context overflow must not read as anything else,
        // even when the body also mentions a limit or a retry.
        assert_eq!(classify("exceeded retry limit: context_length_exceeded (rate limit headers attached)", true), Content);
    }

    #[test]
    fn unexplained_text_fails_safe_once_the_turn_began() {
        for detail in ["turn.failed with no message", "synthetic unrecognised failure 7F3A", "", "codex exited 1"] {
            assert_eq!(classify(detail, true), Unrecognised, "{detail}");
            assert_eq!(classify(detail, false), Binary, "{detail}: nothing ran before the turn");
        }
    }

    #[test]
    fn quota_status_codes_need_a_standalone_token() {
        assert_eq!(classify("request id req_4291a7 failed", true), Unrecognised);
        assert_eq!(classify("status 429", true), Quota);
    }

    #[test]
    fn only_outage_classes_are_provider_side() {
        let provider_side: Vec<_> = [Readiness, Quota, Transport, Auth, Binary, Content, Unrecognised]
            .into_iter().filter(|class| class.is_provider_side()).collect();
        assert_eq!(provider_side, vec![Quota, Transport, Auth, Binary]);
    }

    #[test]
    fn errors_follow_the_module_table() {
        use CapabilityClass::*;
        for capability in [TextOnly, ReadTools, WriteTools, FullAgentic] {
            let may_write = matches!(capability, WriteTools | FullAgentic);
            for class in [Readiness, Quota, Transport, Auth, Binary, Content, Unrecognised] {
                let error = turn_error("codex", class, capability, "synthetic detail JARVIS_READINESS:mcp_tools PRIVATE".into());
                let typed = ReasonerError::find_in(&error);
                match class {
                    Readiness => assert!(matches!(typed, Some(ReasonerError::Local { .. }))),
                    Quota => assert!(matches!(typed, Some(ReasonerError::RateLimited { .. }))),
                    Transport | Auth | Binary => assert!(matches!(typed, Some(ReasonerError::Unavailable { .. }))),
                    Unrecognised if !may_write => assert!(matches!(typed, Some(ReasonerError::Unavailable { .. })),
                        "{capability:?}: an unrecognised text/read failure is an outage"),
                    Content | Unrecognised => {
                        assert!(typed.is_none(), "{class:?} on {capability:?} must stay untyped");
                        assert_eq!(error.downcast_ref::<TurnFailure>().map(|f| f.class), Some(class));
                    }
                }
                if class == Readiness {
                    assert!(!error.to_string().contains("PRIVATE"), "{error}");
                    assert!(error.to_string().contains("MCP tool is missing"), "{error}");
                }
            }
        }
    }

    /// #1069 review H1, pinned against the table: codex 0.154 failures the
    /// table does not know, on each capability class.
    #[test]
    fn routes_unrecognised_by_capability() {
        use CapabilityClass::*;
        for detail in ["synthetic unrecognised failure 7F3A", "unexpected status 404 Not Found: model_not_found"] {
            let class = classify(detail, true);
            assert_eq!(class, Unrecognised, "{detail}");
            for capability in [TextOnly, ReadTools] {
                assert!(ReasonerError::find_in(&turn_error("codex", class, capability, detail.into()))
                    .is_some_and(ReasonerError::is_provider_side), "{capability:?}");
            }
            for capability in [WriteTools, FullAgentic] {
                assert!(ReasonerError::find_in(&turn_error("codex", class, capability, detail.into())).is_none(), "{capability:?}");
            }
        }
    }

    #[test]
    fn an_explicit_status_outranks_body_text_but_400_403_404_do_not_decide() {
        assert_eq!(classify("unexpected status 503 Service Unavailable: context window", true), Transport);
        assert_eq!(classify("HTTP/1.1 502 upstream said: usage policy", true), Transport);
        assert_eq!(classify("unexpected status 400 Bad Request: context_length_exceeded", true), Content);
        assert_eq!(classify("unexpected status 403 Forbidden: This request has been flagged for possible cybersecurity risk.", true), Content);
        assert_eq!(classify("request id 5031 failed with a synthetic reason", true), Unrecognised);
    }
}
