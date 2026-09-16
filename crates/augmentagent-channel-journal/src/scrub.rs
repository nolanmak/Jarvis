//! #1055 — journal privacy: excluded topics + conservative secret scrubbing.
//!
//! The owner keeps a dedicated journal topic as a password vault. Entries in
//! an excluded topic are consumed (so sync never retries them) but store
//! ZERO bytes anywhere — no `journal/` page, no `journal/history/` revision,
//! no LLM ingest. Every other entry passes through `scrub_secrets` before
//! any byte is written. Deliberately conservative: prose must never be
//! mangled, so only unambiguous secret shapes are redacted.

use std::sync::OnceLock;

use regex::Regex;

/// Built-in excluded topic names, matched case-insensitively against the
/// trimmed topic. The lock emoji is the owner's password-vault topic
/// convention. Extend (never replace) via `AUGMENTAGENT_JOURNAL_EXCLUDE_TOPICS`.
const DEFAULT_EXCLUDED: &[&str] = &["password", "passwords", "passwd", "pwd", "credentials", "🔒"];

/// Is this topic on the never-store list?
pub fn is_excluded_topic(topic: Option<&str>, extra: &[String]) -> bool {
    let Some(t) = topic.map(str::trim).filter(|t| !t.is_empty()) else {
        return false;
    };
    let lower = t.to_lowercase();
    DEFAULT_EXCLUDED.iter().any(|d| lower == *d)
        || extra.iter().any(|e| lower == e.trim().to_lowercase())
}

/// Comma-separated additional excluded topics from
/// `AUGMENTAGENT_JOURNAL_EXCLUDE_TOPICS` (exact match, case-insensitive).
pub fn exclude_topics_from_env() -> Vec<String> {
    std::env::var("AUGMENTAGENT_JOURNAL_EXCLUDE_TOPICS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Redact unambiguous secrets from entry text; returns the scrubbed text and
/// how many redactions were made. Covers: `password/passwd/pwd/passcode/pin`
/// labels followed by `:`/`=` and a value; provider token shapes
/// (`ghp_…`, `xox[baprs]-…`, `sk-…`, `AIza…`); PEM private-key blocks.
pub fn scrub_secrets(text: &str) -> (String, usize) {
    let mut count = 0usize;
    let pass1 = key_block_re().replace_all(text, |_: &regex::Captures| {
        count += 1;
        "[redacted-private-key]".to_string()
    });
    let pass2 = label_re().replace_all(&pass1, |c: &regex::Captures| {
        count += 1;
        format!("{}{}[redacted]", &c[1], &c[2])
    });
    let pass3 = token_re().replace_all(&pass2, |_: &regex::Captures| {
        count += 1;
        "[redacted-token]".to_string()
    });
    (pass3.into_owned(), count)
}

fn label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(password|passwd|pwd|passcode|pin)\b(\s*[:=]\s*)(\S+)").unwrap()
    })
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"ghp_[A-Za-z0-9]{30,}|xox[baprs]-[A-Za-z0-9-]{10,}|sk-[A-Za-z0-9]{20,}|AIza[A-Za-z0-9_-]{30,}",
        )
        .unwrap()
    })
}

fn key_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(-----END [A-Z ]*PRIVATE KEY-----|\z)")
            .unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_topic_matches_defaults_and_env_extras_case_insensitively() {
        assert!(is_excluded_topic(Some("Passwords"), &[]));
        assert!(is_excluded_topic(Some("  PASSWORD  "), &[]));
        assert!(is_excluded_topic(Some("🔒"), &[]));
        assert!(!is_excluded_topic(Some("Morning Journal"), &[]));
        assert!(!is_excluded_topic(Some("Gratitude"), &[]));
        assert!(!is_excluded_topic(None, &[]));
        let extra = vec!["Vault".to_string()];
        assert!(is_excluded_topic(Some("vault"), &extra));
        assert!(!is_excluded_topic(Some("vaults"), &extra), "exact match only");
    }

    #[test]
    fn scrub_redacts_labeled_passwords_but_not_prose() {
        let (out, n) = scrub_secrets("my password: synthetic123\nnice day at the lake");
        assert_eq!(n, 1);
        assert!(out.contains("password: [redacted]"), "{out}");
        assert!(!out.contains("synthetic123"), "{out}");
        assert!(out.contains("nice day at the lake"), "{out}");
        // labels without a separator, and prose containing label-ish words,
        // are left alone
        let (out, n) = scrub_secrets("I passed the word around. The pin board was full.");
        assert_eq!(n, 0);
        assert!(out.contains("pin board"), "{out}");
    }

    #[test]
    fn scrub_redacts_token_shapes_and_key_blocks() {
        let (out, n) = scrub_secrets(
            "token ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA123456 and\n-----BEGIN RSA PRIVATE KEY-----\nMIIsynthetic\n-----END RSA PRIVATE KEY-----\ndone", // pii-ok: synthetic fixture, the shape this module exists to redact
        );
        assert!(n >= 2, "{out}");
        assert!(out.contains("[redacted-token]"), "{out}");
        assert!(out.contains("[redacted-private-key]"), "{out}");
        assert!(!out.contains("ghp_"), "{out}");
        assert!(!out.contains("MIIsynthetic"), "{out}");
        assert!(out.ends_with("done"), "{out}");
    }

    #[test]
    fn scrub_is_a_noop_on_clean_prose() {
        let text = "Grateful for sleep and Philly. Read, meditate before bed.";
        let (out, n) = scrub_secrets(text);
        assert_eq!(n, 0);
        assert_eq!(out, text);
    }
}
