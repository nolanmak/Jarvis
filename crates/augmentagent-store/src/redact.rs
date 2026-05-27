//! Forward-only secret/PII redaction at the persistence boundary.
//!
//! Forward-only; existing rows are NOT rewritten. Mask is applied at the
//! persistence boundary only — channels still see plaintext for API calls.
//!
//! The Node dashboard and the Rust daemon both write into the shared
//! `data.db`, but channel logic, action queues, and email bodies funnel
//! through Rust store writes that accept caller-supplied JSON/HTTP-body
//! text. Anything that lands in those columns is run through [`mask`]
//! before being bound to a SQL parameter, so a stolen DB snapshot does
//! not double as a credential dump.
//!
//! Patterns covered:
//! - HTTP: `Authorization: Bearer <tok>` / `Authorization: Basic <tok>`
//! - JSON string values whose key (case-insensitive) is one of
//!   `api_key`, `apiKey`, `access_token`, `accessToken`, `refresh_token`,
//!   `refreshToken`, `password`, `client_secret`, `clientSecret`
//! - Raw token shapes anywhere in the text: `sk-…`, `xoxb-…`, `ghp_…`,
//!   `AIza…`, `ak_…`
//!
//! All matches are replaced with the literal string `[REDACTED]`. Keys are
//! preserved; only the value is masked.

use std::borrow::Cow;

use once_cell::sync::Lazy;
use regex::Regex;

/// Sentinel inserted in place of any matched secret value.
const PLACEHOLDER: &str = "[REDACTED]";

/// Ordered list of (regex, replacement-template) pairs. Order matters only
/// for cosmetic reasons (more specific patterns first); every pattern is
/// safe to run regardless of which others have already matched.
struct Rule {
    re: Regex,
    /// Replacement string passed to `Regex::replace_all`. Uses `$1` / `$2`
    /// backrefs where we need to preserve a prefix (e.g. `Authorization:
    /// Bearer ` or a JSON key).
    rewrite: String,
}

static RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    vec![
        // --- HTTP Authorization headers --------------------------------
        // `Authorization: Bearer <tok>` and `Authorization: Basic <tok>`.
        // Captures the scheme prefix so we can keep it in the output.
        Rule {
            re: Regex::new(
                r"(?i)(Authorization\s*:\s*(?:Bearer|Basic))\s+[A-Za-z0-9._\-+/=]+",
            )
            .unwrap(),
            rewrite: format!("$1 {PLACEHOLDER}"),
        },
        // --- JSON string-valued secret fields --------------------------
        // Case-insensitive key match. The value can be any JSON string
        // (no embedded unescaped quotes, which matches normal JSON output).
        // Allowed key set is enumerated explicitly so we don't accidentally
        // mask unrelated columns like `passwords_required`.
        Rule {
            re: Regex::new(
                r#"(?i)("(?:api_key|apiKey|access_token|accessToken|refresh_token|refreshToken|password|client_secret|clientSecret)"\s*:\s*")[^"\\]*(?:\\.[^"\\]*)*(")"#,
            )
            .unwrap(),
            rewrite: format!("$1{PLACEHOLDER}$2"),
        },
        // --- Raw token shapes ------------------------------------------
        // OpenAI-style sk- keys.
        Rule {
            re: Regex::new(r"sk-[A-Za-z0-9_\-]{16,}").unwrap(),
            rewrite: PLACEHOLDER.to_string(),
        },
        // Slack bot tokens.
        Rule {
            re: Regex::new(r"xoxb-[A-Za-z0-9\-]{20,}").unwrap(),
            rewrite: PLACEHOLDER.to_string(),
        },
        // GitHub personal access tokens.
        Rule {
            re: Regex::new(r"ghp_[A-Za-z0-9]{20,}").unwrap(),
            rewrite: PLACEHOLDER.to_string(),
        },
        // Google API keys.
        Rule {
            re: Regex::new(r"AIza[A-Za-z0-9_\-]{20,}").unwrap(),
            rewrite: PLACEHOLDER.to_string(),
        },
        // Generic `ak_` API keys (Augmenta-style).
        Rule {
            re: Regex::new(r"ak_[A-Za-z0-9_\-]{16,}").unwrap(),
            rewrite: PLACEHOLDER.to_string(),
        },
    ]
});

/// Mask any secret-shaped substrings inside `s`, returning a borrowed
/// view when nothing matched (zero-alloc fast path) or an owned `String`
/// otherwise.
///
/// Safe to call on arbitrary text — HTTP bodies, JSON blobs, free-form
/// notes. Multiple rules can fire on the same input; each rule runs over
/// the result of the previous rule.
pub fn mask(s: &str) -> Cow<'_, str> {
    let mut cur: Cow<'_, str> = Cow::Borrowed(s);
    for rule in RULES.iter() {
        // `Regex::replace_all` already returns a `Cow::Borrowed(s)` when
        // there are no matches, so the no-op case threads through without
        // allocating.
        match rule.re.replace_all(&cur, rule.rewrite.as_str()) {
            Cow::Borrowed(_) => {
                // No change at this rule; keep `cur` as-is.
            }
            Cow::Owned(new) => {
                cur = Cow::Owned(new);
            }
        }
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn masks_authorization_bearer() {
        let input = "Authorization: Bearer sk-fake-1234567890abcdef";
        let out = mask(input);
        assert_eq!(out, "Authorization: Bearer [REDACTED]");
        assert!(matches!(out, Cow::Owned(_)));
    }

    #[test]
    fn masks_authorization_basic() {
        let input = "Authorization: Basic dXNlcjpwYXNzd29yZGZha2VmYWtl";
        let out = mask(input);
        assert_eq!(out, "Authorization: Basic [REDACTED]");
    }

    #[test]
    fn masks_json_api_key_snake_case() {
        let input = r#"{"api_key":"ak_fakefakefakefake"}"#;
        let out = mask(input);
        assert_eq!(out, r#"{"api_key":"[REDACTED]"}"#);
    }

    #[test]
    fn masks_json_apikey_camel_case_with_spacing() {
        let input = r#"{"apiKey": "AIzaSyFAKEFAKEFAKEFAKEFAKE"}"#;
        let out = mask(input);
        // The JSON-field rule fires first and masks the value; the raw
        // AIza rule would also have masked it, but only one mask remains.
        assert_eq!(out, r#"{"apiKey": "[REDACTED]"}"#);
    }

    #[test]
    fn masks_json_access_token() {
        let input = r#"{"access_token":"ghp_fakefakefakefakefake"}"#;
        let out = mask(input);
        assert_eq!(out, r#"{"access_token":"[REDACTED]"}"#);
    }

    #[test]
    fn masks_json_refresh_token_camel() {
        let input = r#"{"refreshToken":"opaque-refresh-token-value-xyz"}"#;
        let out = mask(input);
        assert_eq!(out, r#"{"refreshToken":"[REDACTED]"}"#);
    }

    #[test]
    fn masks_json_password_and_client_secret() {
        let input = r#"{"password":"hunter2","client_secret":"supersecretsupersecret"}"#;
        let out = mask(input);
        assert_eq!(
            out,
            r#"{"password":"[REDACTED]","client_secret":"[REDACTED]"}"#
        );
    }

    #[test]
    fn masks_raw_slack_token() {
        let input = "token=xoxb-1234567890-abcdefghij-FAKE";
        let out = mask(input);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("xoxb-1234567890"));
    }

    #[test]
    fn masks_raw_github_pat() {
        let input = "saw ghp_FAKEFAKEFAKEFAKEFAKEFAKEFAKE in logs";
        let out = mask(input);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("ghp_FAKE"));
    }

    #[test]
    fn innocent_text_is_borrowed() {
        let input = "the api should respond with json";
        let out = mask(input);
        assert_eq!(out, "the api should respond with json");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn empty_string_is_borrowed() {
        let out = mask("");
        assert_eq!(out, "");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn json_key_only_no_match() {
        // The substring `api_key` appears, but not in a JSON-key context
        // (it's not followed by `":"…"`). The raw-token rules also have
        // no shape to match. Should stay borrowed.
        let input = "documentation: the api_key parameter is required";
        let out = mask(input);
        assert_eq!(out, input);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn similarly_named_key_passwords_required_not_masked() {
        // The `password` rule must use a key-anchored regex, not a bare
        // substring match, so `passwords_required` and friends stay clean.
        let input = r#"{"passwords_required":true}"#;
        let out = mask(input);
        assert_eq!(out, input);
    }

    #[test]
    fn multiple_secrets_in_one_blob() {
        let input = concat!(
            "POST /v1/things\n",
            "Authorization: Bearer sk-fake-abcdefghijklmnop\n",
            "Content-Type: application/json\n\n",
            r#"{"api_key":"ak_fakefakefakefake","name":"widget"}"#,
        );
        let out = mask(input);
        assert!(out.contains("Authorization: Bearer [REDACTED]"));
        assert!(out.contains(r#""api_key":"[REDACTED]""#));
        assert!(out.contains(r#""name":"widget""#));
    }

    #[test]
    fn redacts_4kb_body_with_one_match_quickly() {
        // Bench-style sanity check (NOT criterion): a 4 KB body with one
        // embedded match should redact in well under 50µs on the build
        // host. The 50µs target is the release-build budget — debug
        // builds are ~5–10x slower because the regex engine isn't
        // inlined/optimized — so we apply a looser threshold under
        // `cfg(debug_assertions)` to keep `cargo test` green while still
        // catching pathological regressions.
        let filler = "lorem ipsum dolor sit amet ".repeat(150); // ~4 KB
        let mut body = String::with_capacity(filler.len() + 64);
        body.push_str(&filler[..2000]);
        body.push_str("Authorization: Bearer sk-fake-1234567890abcdef\n");
        body.push_str(&filler[2000..]);

        // Warm up the lazy regex compile + JIT cache so the timed run
        // measures steady-state work, not first-touch initialization.
        let _ = mask(&body);

        let start = Instant::now();
        let out = mask(&body);
        let elapsed = start.elapsed();

        assert!(out.contains("[REDACTED]"));

        let budget_us: u128 = if cfg!(debug_assertions) { 2000 } else { 50 };
        assert!(
            elapsed.as_micros() < budget_us,
            "redact took {}µs on 4 KB body, expected <{}µs (debug_assertions={})",
            elapsed.as_micros(),
            budget_us,
            cfg!(debug_assertions),
        );
    }
}
