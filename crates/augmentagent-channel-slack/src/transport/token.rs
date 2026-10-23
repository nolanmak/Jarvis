//! Secret wrappers for the two Slack credentials the transport uses.
//!
//! Neither type implements `Display`, `Serialize` or a transparent `Debug`:
//! the only way to read the secret is [`AppLevelToken::expose_secret`] /
//! [`BotToken::expose_secret`], which keeps accidental `{:?}` logging from
//! leaking credentials. `tests/transport_events.rs` pins this.

use std::fmt;

/// App-level token (`xapp-…`) used only for `apps.connections.open`.
#[derive(Clone, PartialEq, Eq)]
pub struct AppLevelToken(String);

/// Bot user OAuth token (`xoxb-…`) used for Web API calls.
#[derive(Clone, PartialEq, Eq)]
pub struct BotToken(String);

macro_rules! secret_impls {
    ($t:ident, $label:literal) => {
        impl $t {
            pub fn new(secret: impl Into<String>) -> Self {
                Self(secret.into())
            }

            /// The raw secret. Only pass this to an `Authorization` header.
            pub fn expose_secret(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($label, "([redacted])"))
            }
        }
    };
}

secret_impls!(AppLevelToken, "AppLevelToken");
secret_impls!(BotToken, "BotToken");

/// Strip anything that looks like a Slack ticket or token out of a string
/// that may end up in a log line or an error message.
///
/// Socket Mode URLs carry a one-time `ticket=` query parameter and tokens
/// start with `xapp-`/`xox?-`; both are replaced with `[redacted]`.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = find_secret_start(rest) {
        out.push_str(&rest[..idx]);
        out.push_str("[redacted]");
        let tail = &rest[idx..];
        let end = tail
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '=')
            })
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn find_secret_start(text: &str) -> Option<usize> {
    let candidates = [
        "ticket=", "xapp-", "xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-", "xoxe-",
    ];
    candidates.iter().filter_map(|c| text.find(c)).min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_tickets_and_tokens() {
        let url = "wss://wss.slack.test/link/?ticket=abc-123&app_id=A00000001";
        let out = redact(url);
        assert!(!out.contains("abc-123"), "{out}");
        assert!(out.contains("app_id=A00000001"), "{out}");
        let msg = "bad token xapp-test-000 for call";
        assert_eq!(redact(msg), "bad token [redacted] for call");
    }
}
