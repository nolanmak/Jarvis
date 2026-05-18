//! Failure-mode detector (#50/#76).
//!
//! Instagram has no `x-ratelimit-*` headers. Throttle/ban state is signalled
//! in-band — either as a JSON body on the private API (`feedback_required`,
//! `checkpoint_required`, …) or as a DOM toast / interstitial on the
//! browser-posting path ("Action Blocked", a captcha iframe, the cookie
//! consent banner, …).
//!
//! This module centralizes the string→[`FailureKind`] classification so the
//! API client (#18/#19) and the browser composer (#50) react consistently,
//! and so the mapping into the channel-core governor's [`HaltReason`] lives
//! in one tested place.

use augmentagent_channel_core::HaltReason;

/// What the agent observed. Ordered roughly worst → least-bad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// `login_required` body (HTTP 403/400 with no 401) or an explicit
    /// `"logged_out"` flag — the *session cookie* is dead. Distinct from
    /// `Challenge`: re-harvesting cookies fixes it without a human clearing
    /// an in-app checkpoint. Maps to [`InstagramError::AuthExpired`].
    LoginRequired,
    /// `checkpoint_required` / `challenge_required` — the *account* is
    /// flagged. Terminal until a human clears it in the app.
    Challenge,
    /// A reCAPTCHA / arkose interstitial in the browser flow.
    Captcha,
    /// "Action Blocked" / "We restrict certain activity" toast — the action
    /// class is soft-blocked for a while.
    ActionBlocked,
    /// `feedback_required` with `spam` / generic 429 — back off hard but the
    /// account itself is probably fine.
    RateLimit,
    /// The cookie-consent / "Allow cookies" interstitial covering the UI on
    /// the browser path. Not a ban — but it blocks automation and must halt
    /// idempotently rather than blindly clicking through.
    CookieBanner,
}

impl FailureKind {
    /// Map into the channel-core governor's halt taxonomy so a detected
    /// failure trips the shared circuit breaker.
    pub fn halt_reason(self) -> HaltReason {
        match self {
            // A dead session is the closest cousin of a login challenge from
            // the governor's perspective — both block the channel until the
            // operator re-authenticates.
            FailureKind::LoginRequired => HaltReason::LoginChallenge,
            FailureKind::Challenge => HaltReason::LoginChallenge,
            FailureKind::Captcha => HaltReason::Captcha,
            FailureKind::ActionBlocked => HaltReason::ActionBlocked,
            FailureKind::RateLimit => HaltReason::RateLimitToast,
            // A cookie banner isn't a ban signal; the closest governor
            // bucket is "action blocked" (the action can't proceed).
            FailureKind::CookieBanner => HaltReason::ActionBlocked,
        }
    }

    /// How long to pause the channel when this is observed, in ms.
    /// `Challenge`/`Captcha` are terminal-ish (24h, needs a human);
    /// rate-limit / action-block self-clear faster (1h per #18).
    pub fn pause_ms(self) -> i64 {
        match self {
            // Re-harvesting cookies is a quick operator action; don't sit on
            // a 24h halt for a dead session — surface it loudly and let the
            // next poll retry once cookies are refreshed (1h backstop).
            FailureKind::LoginRequired => 3600 * 1000,
            FailureKind::Challenge | FailureKind::Captcha => 24 * 3600 * 1000,
            FailureKind::ActionBlocked | FailureKind::RateLimit => 3600 * 1000,
            FailureKind::CookieBanner => 3600 * 1000,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::LoginRequired => "login_required",
            FailureKind::Challenge => "challenge",
            FailureKind::Captcha => "captcha",
            FailureKind::ActionBlocked => "action_blocked",
            FailureKind::RateLimit => "rate_limit",
            FailureKind::CookieBanner => "cookie_banner",
        }
    }
}

/// Classify a private-API HTTP response. `status` is the HTTP code, `body`
/// the (possibly JSON) body text. Returns `None` when nothing ban-ish is
/// detected (caller treats as a generic API error).
pub fn classify_body(status: u16, body: &str) -> Option<FailureKind> {
    let lower = body.to_ascii_lowercase();
    // Order matters: a checkpoint/challenge body is the worst case and
    // sometimes co-occurs with `login_required`-ish phrasing — classify it
    // first so we never under-react.
    if lower.contains("checkpoint_required")
        || lower.contains("challenge_required")
        || lower.contains("checkpoint_url")
    {
        return Some(FailureKind::Challenge);
    }
    // Dead session: IG signals this as a `login_required` message body
    // (often HTTP 403/400 — NOT always a clean 401), or a top-level
    // `"logged_in_user": null` / `"logged_out"` marker on the inbox probe.
    if lower.contains("login_required")
        || lower.contains("\"logged_out\"")
        || lower.contains("\"logged_in_user\":null")
        || lower.contains("\"logged_in_user\": null")
        || lower.contains("please log in")
    {
        return Some(FailureKind::LoginRequired);
    }
    if lower.contains("feedback_required") || lower.contains("\"spam\"") {
        return Some(FailureKind::ActionBlocked);
    }
    if lower.contains("\"lock\"") || lower.contains("please wait a few minutes") {
        return Some(FailureKind::ActionBlocked);
    }
    if status == 429
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("too many requests")
    {
        return Some(FailureKind::RateLimit);
    }
    None
}

/// Classify a DOM snapshot from the browser-posting path. `page_url` +
/// visible `body_text` (innerText of the document) are scanned for the
/// known interstitial fingerprints.
pub fn classify_dom(page_url: &str, body_text: &str) -> Option<FailureKind> {
    let url = page_url.to_ascii_lowercase();
    let text = body_text.to_ascii_lowercase();

    if url.contains("/challenge/") || url.contains("/accounts/suspended") {
        return Some(FailureKind::Challenge);
    }
    if text.contains("recaptcha")
        || text.contains("confirm you're a human")
        || text.contains("confirm you are a human")
    {
        return Some(FailureKind::Captcha);
    }
    if text.contains("action blocked")
        || text.contains("we restrict certain activity")
        || text.contains("try again later")
        || text.contains("we limit how often")
    {
        return Some(FailureKind::ActionBlocked);
    }
    if text.contains("allow the use of cookies")
        || text.contains("allow all cookies")
        || text.contains("accept cookies")
    {
        return Some(FailureKind::CookieBanner);
    }
    if text.contains("please wait a few minutes before you try again") {
        return Some(FailureKind::RateLimit);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_checkpoint_is_challenge() {
        assert_eq!(
            classify_body(400, r#"{"message":"checkpoint_required"}"#),
            Some(FailureKind::Challenge)
        );
        assert_eq!(
            classify_body(400, r#"{"message":"challenge_required"}"#),
            Some(FailureKind::Challenge)
        );
    }

    #[test]
    fn body_feedback_required_is_action_blocked() {
        assert_eq!(
            classify_body(400, r#"{"message":"feedback_required","spam":true}"#),
            Some(FailureKind::ActionBlocked)
        );
    }

    #[test]
    fn body_429_is_rate_limit() {
        assert_eq!(classify_body(429, "{}"), Some(FailureKind::RateLimit));
    }

    #[test]
    fn body_login_required_is_login_required() {
        // The classic dead-session body — often HTTP 403, not 401.
        assert_eq!(
            classify_body(403, r#"{"message":"login_required","status":"fail"}"#),
            Some(FailureKind::LoginRequired)
        );
        // The inbox-probe variant: a 200 with a null viewer.
        assert_eq!(
            classify_body(200, r#"{"logged_in_user": null, "status":"ok"}"#),
            Some(FailureKind::LoginRequired)
        );
        assert_eq!(
            classify_body(400, r#"{"message":"Please log in to continue."}"#),
            Some(FailureKind::LoginRequired)
        );
    }

    #[test]
    fn login_required_outranks_feedback_in_same_body() {
        // A body that mentions both must classify as the worse re-auth case,
        // not a soft action-block.
        assert_eq!(
            classify_body(403, r#"{"message":"login_required","feedback_required":true}"#),
            Some(FailureKind::LoginRequired)
        );
    }

    #[test]
    fn body_too_many_requests_phrase_is_rate_limit() {
        assert_eq!(
            classify_body(400, r#"{"message":"Too many requests, try later"}"#),
            Some(FailureKind::RateLimit)
        );
    }

    #[test]
    fn login_required_halt_and_pause_mapping() {
        assert!(matches!(
            FailureKind::LoginRequired.halt_reason(),
            HaltReason::LoginChallenge
        ));
        // Quick operator fix (re-harvest) ⇒ 1h backstop, not the 24h
        // checkpoint hold.
        assert_eq!(FailureKind::LoginRequired.pause_ms(), 3600 * 1000);
        assert_eq!(FailureKind::LoginRequired.as_str(), "login_required");
    }

    #[test]
    fn body_clean_is_none() {
        assert_eq!(classify_body(500, r#"{"message":"server boom"}"#), None);
    }

    #[test]
    fn dom_challenge_url() {
        assert_eq!(
            classify_dom("https://www.instagram.com/challenge/?next=/", "..."),
            Some(FailureKind::Challenge)
        );
    }

    #[test]
    fn dom_captcha_text() {
        assert_eq!(
            classify_dom("https://instagram.com/", "Please confirm you're a human"),
            Some(FailureKind::Captcha)
        );
    }

    #[test]
    fn dom_action_blocked_text() {
        assert_eq!(
            classify_dom("https://instagram.com/", "Action Blocked. Try again later"),
            Some(FailureKind::ActionBlocked)
        );
    }

    #[test]
    fn dom_cookie_banner() {
        assert_eq!(
            classify_dom("https://instagram.com/", "Allow all cookies"),
            Some(FailureKind::CookieBanner)
        );
    }

    #[test]
    fn halt_reason_mapping() {
        assert!(matches!(
            FailureKind::Challenge.halt_reason(),
            HaltReason::LoginChallenge
        ));
        assert!(matches!(
            FailureKind::RateLimit.halt_reason(),
            HaltReason::RateLimitToast
        ));
        assert_eq!(FailureKind::Challenge.pause_ms(), 24 * 3600 * 1000);
        assert_eq!(FailureKind::RateLimit.pause_ms(), 3600 * 1000);
    }
}
