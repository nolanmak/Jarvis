//! Fail closed on recognizable private payloads before publishing to GitHub.
//! This is a pattern backstop, not a detector for every private sentence.
use std::sync::OnceLock;

use regex::Regex;

pub fn validate(title: &str, body: &str) -> anyhow::Result<()> {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    static RAW: OnceLock<Regex> = OnceLock::new();
    let email =
        EMAIL.get_or_init(|| Regex::new(r"(?i)[a-z0-9._%+-]+@([a-z0-9.-]+\.[a-z]{2,})").unwrap());
    let raw = RAW.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)user.s words|original program|repair program|urn:li:|/home/[^/\s]+|/Users/[^/\s]+|",
            r"\+[1-9][0-9]{7,14}\b|\b[0-9]{3}[-. ][0-9]{3}[-. ][0-9]{4}\b|",
            r"\([0-9]{3}\)[ .-]*[0-9]{3}[ .-][0-9]{4}|",
            r#"(?:message|thread|profile)[ _-]?id["']?\s*[:=]\s*["']?(?:[0-9a-f]{12,}|urn:)|"#,
            r"ghp_[a-z0-9]{20,}|github_pat_[a-z0-9_]{20,}|sk-(?:proj-|ant-)?[a-z0-9_-]{20,}|",
            r"xox[baprs]-[a-z0-9-]{10,}|AIza[a-z0-9_-]{20,}|-----BEGIN [A-Z ]*PRIVATE KEY-----"
        )).unwrap()
    });
    for text in [title, body] {
        if text.contains('\0')
            || raw.is_match(text)
            || augmentagent_store::redact::mask(text).as_ref() != text
        {
            anyhow::bail!("public report blocked: use a synthetic technical summary; private payload detected (values withheld)");
        }
        for captures in email.captures_iter(text) {
            let domain = captures[1].to_ascii_lowercase();
            let reserved = [
                "example.com",
                "example.net",
                "example.org",
                "example",
                "test",
                "invalid",
            ]
            .iter()
            .any(|d| domain == *d || domain.ends_with(&format!(".{d}")));
            if !reserved && domain != "users.noreply.github.com" {
                anyhow::bail!("public report blocked: replace email addresses with reserved example domains (values withheld)");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_synthetic_reports() {
        assert!(validate(
            "Approval card labels are reversed",
            "To: person@example.com; account@example.invalid"
        )
        .is_ok());
    }

    #[test]
    fn rejects_private_shapes_in_title_or_body_without_echoing_them() {
        let cases = [
            "person".to_owned() + "@" + "gmail.com",
            "person".to_owned() + "@" + "example.com.attacker.org",
            "github_pat_".to_owned() + &"x".repeat(40),
            "phone: ".to_owned() + "+1" + "2025550199",
            "phone: (202) ".to_owned() + "555-0199",
            "urn:li:messagingMessage:private".into(),
            "messageId: abcdef1234567890".into(),
            "User's words: private quote".into(),
            "Original program: private message".into(),
            "safe\0hidden".into(),
        ];
        for private in cases {
            for (title, body) in [(&private[..], "safe"), ("safe", &private[..])] {
                let err = validate(title, body).unwrap_err().to_string();
                assert!(!err.contains(&private));
            }
        }
    }
}
