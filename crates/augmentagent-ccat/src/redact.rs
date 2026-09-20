use regex::Regex;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedPayload {
    pub text: String,
    pub sha256: String,
}

/// Remove direct identifiers and secrets before a payload crosses the provider boundary.
pub fn redact(input: &str) -> RedactedPayload {
    let mut text = input.to_string();
    let patterns = [
        (
            r"(?i)-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            "[REDACTED_PRIVATE_KEY]",
        ),
        (
            r"(?i)\b(?:ghp|github_pat|xox[baprs]|sk|tz)_[A-Za-z0-9_-]{10,}\b",
            "[REDACTED_SECRET]",
        ),
        (
            r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b",
            "[REDACTED_EMAIL]",
        ),
        (
            r"\b(?:\+?1[-. ]?)?(?:\(?[0-9]{3}\)?[-. ]?)[0-9]{3}[-. ][0-9]{4}\b",
            "[REDACTED_PHONE]",
        ),
        (r"(?m)(?:/home|/Users)/[^/\\s]+", "[REDACTED_HOME]"),
    ];
    for (pattern, replacement) in patterns {
        text = Regex::new(pattern)
            .expect("static regex")
            .replace_all(&text, replacement)
            .into_owned();
    }
    let sha256 = hex::encode(Sha256::digest(text.as_bytes()));
    RedactedPayload { text, sha256 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_identifiers_do_not_cross_boundary() {
        let out = redact("email alice@example.com phone 212-555-0199 key tz_abcdefghijklmnop path /home/alice/private"); // pii-ok: synthetic redaction fixture
        assert!(!out.text.contains("alice@example.com"));
        assert!(!out.text.contains("212-555-0199"));
        assert!(!out.text.contains("tz_abcdefghijklmnop"));
        assert!(!out.text.contains("/home/alice"));
    }
    #[test]
    fn payload_hash_is_stable_for_identical_redaction() {
        assert_eq!(redact("a@b.com").sha256, redact("a@b.com").sha256); // pii-ok: synthetic redaction fixture
    }
}
