//! Email-signature parsing → backfill role/title/company/phone (#64).
//!
//! `tone.rs::clean_sent_body` deliberately *discards* signature blocks (it
//! truncates at the RFC-3676 `\n-- \n` delimiter and the
//! `Sent from my …` trailer) because trailing sig text would distort the
//! voice profile. This module is the inverse: it **keeps** the sig block and
//! mines it for structured contact facts. It is fully additive — it never
//! calls into nor changes `tone.rs`, so the production tone pipeline stays
//! byte-identical (the zero-regression constraint).
//!
//! Pipeline (issue §"Proposed architecture"):
//! 1. [`strip_quoted_reply`] — drop the reply chain (a *local copy* of the
//!    quoted-reply rule, intentionally not shared with `tone.rs` so a future
//!    edit here can never regress the prod sent-mail cleaner).
//! 2. [`detect_signature_block`] — pure regex + line-density heuristic.
//!    Returns 0 or 1 candidate block. Unit-tested in isolation.
//! 3. [`SignatureExtractor`] — LLM field extraction over the existing
//!    `Reasoner` (Claude CLI) path: strict JSON with per-field confidence,
//!    retry-on-parse-fail, empty-stays-empty. A pure regex fallback covers
//!    the no-LLM / parse-fail path so the heuristic alone still yields
//!    high-confidence phones/URLs.
//! 4. [`signature_patch`] — fill-blanks-only wiki patch (shared merger).

use serde::{Deserialize, Serialize};

use augmentagent_channel_core::reasoner::{Reasoner, ReasonerOpts};
use augmentagent_wiki::PersonPatch;

/// Strip a Gmail/Outlook quoted-reply chain. **Local copy** of the rule in
/// `tone.rs` (lines flagged "Strip Gmail's quoted-reply preamble"): keeping
/// it separate is deliberate — #64 must not edit the prod tone cleaner.
/// Unlike `clean_sent_body`, this stops *before* the sig delimiter so the
/// signature survives for extraction.
pub fn strip_quoted_reply(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n");
    let s = if let Some(on_pos) = normalized.find("\nOn ") {
        if normalized[on_pos..].contains(" wrote:") {
            normalized[..on_pos].to_string()
        } else {
            normalized
        }
    } else {
        normalized
    };
    // Drop `>`-quoted lines but DO keep the sig delimiter + sig body.
    s.lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A detected signature candidate: the raw block text + why we think it's a
/// signature (for the dry-run report / debugging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureBlock {
    pub text: String,
    /// `delimiter` (explicit `-- `), `closing` (Best,/Regards,/Thanks,),
    /// or `density` (trailing low-word-count lines with contact tokens).
    pub reason: &'static str,
}

/// Closing salutations that mark the start of a sig block when followed by a
/// short name/contact tail.
const CLOSINGS: &[&str] = &[
    "best,", "best regards,", "regards,", "kind regards,", "warm regards,",
    "thanks,", "thank you,", "cheers,", "sincerely,", "best wishes,",
    "talk soon,", "warmly,", "respectfully,",
];

/// Mobile-mail trailers — their own (1-line) signature.
const MOBILE_TRAILERS: &[&str] = &[
    "sent from my iphone",
    "sent from my ipad",
    "sent from my android",
    "sent from my mobile",
    "sent from my galaxy",
    "get outlook for",
];

/// Detect 0 or 1 signature block in a (quoted-reply-stripped) body.
///
/// Strategy, highest-confidence first:
/// 1. Explicit RFC-3676 delimiter line `-- ` → everything after is the sig.
/// 2. A closing salutation line near the end → sig starts there.
/// 3. Line-density: the trailing run of short lines (≤ ~6 words) that carry
///    contact tokens (phone digits, URL, `@`, title words).
///
/// Returns `None` when nothing convincing is found (no false sig on a plain
/// prose email — empty stays empty downstream).
pub fn detect_signature_block(body: &str) -> Option<SignatureBlock> {
    let body = body.trim_end();
    if body.is_empty() {
        return None;
    }
    let lines: Vec<&str> = body.lines().collect();

    // 1. Explicit delimiter.
    for (i, l) in lines.iter().enumerate() {
        if l.trim_end() == "--" || l.trim_end() == "-- " || l.trim() == "--" {
            let block = lines[i + 1..].join("\n").trim().to_string();
            if !block.is_empty() {
                return Some(SignatureBlock {
                    text: block,
                    reason: "delimiter",
                });
            }
        }
    }

    // 2. Mobile trailer (anywhere in the last 3 lines).
    let tail_start = lines.len().saturating_sub(3);
    for (i, l) in lines.iter().enumerate().skip(tail_start) {
        let low = l.trim().to_ascii_lowercase();
        if MOBILE_TRAILERS.iter().any(|t| low.starts_with(t)) {
            return Some(SignatureBlock {
                text: lines[i..].join("\n").trim().to_string(),
                reason: "delimiter",
            });
        }
    }

    // 3. Closing salutation in the last ~8 lines.
    let window_start = lines.len().saturating_sub(8);
    for (i, l) in lines.iter().enumerate().skip(window_start) {
        let low = l.trim().to_ascii_lowercase();
        if CLOSINGS.iter().any(|c| low == *c || low.starts_with(c)) {
            // Sig is the closing + everything after; require at least one
            // contact-ish or name line after the closing so "Thanks," at the
            // end of a sentence-less line isn't a false positive.
            let after = &lines[i + 1..];
            if after.iter().any(|x| looks_like_contact(x) || is_name_line(x)) {
                let block = lines[i..].join("\n").trim().to_string();
                return Some(SignatureBlock {
                    text: block,
                    reason: "closing",
                });
            }
        }
    }

    // 4. Trailing low-density run carrying contact tokens.
    let mut start = lines.len();
    let mut contact_hits = 0;
    for i in (0..lines.len()).rev() {
        let l = lines[i].trim();
        if l.is_empty() {
            if lines.len() - i > 6 {
                break;
            }
            continue;
        }
        let words = l.split_whitespace().count();
        if words <= 7 && (looks_like_contact(l) || is_name_line(l)) {
            if looks_like_contact(l) {
                contact_hits += 1;
            }
            start = i;
        } else {
            break;
        }
    }
    if start < lines.len() && contact_hits >= 1 && lines.len() - start <= 8 {
        let block = lines[start..].join("\n").trim().to_string();
        if !block.is_empty() {
            return Some(SignatureBlock {
                text: block,
                reason: "density",
            });
        }
    }

    None
}

/// A line that carries a contact token: phone-ish digit run, URL, email,
/// or a known title keyword.
fn looks_like_contact(line: &str) -> bool {
    let l = line.trim();
    if l.is_empty() {
        return false;
    }
    let low = l.to_ascii_lowercase();
    let digits = l.chars().filter(|c| c.is_ascii_digit()).count();
    let phoneish = digits >= 7
        && l.chars()
            .all(|c| c.is_ascii_digit() || " +-().x".contains(c) || c == '\t');
    phoneish
        || low.contains("http://")
        || low.contains("https://")
        || low.contains("www.")
        || (l.contains('@') && l.contains('.') && !l.contains(' '))
        || TITLE_WORDS.iter().any(|w| low.contains(w))
}

const TITLE_WORDS: &[&str] = &[
    "ceo", "cto", "cfo", "coo", "founder", "co-founder", "engineer",
    "developer", "manager", "director", "president", "vp ", "head of",
    "lead", "partner", "consultant", "designer", "analyst", "officer",
    "principal", "architect",
];

/// A short, capitalized, punctuation-light line — likely a person/company
/// name in a sig.
fn is_name_line(line: &str) -> bool {
    let l = line.trim();
    if l.is_empty() || l.len() > 60 {
        return false;
    }
    let words: Vec<&str> = l.split_whitespace().collect();
    if words.is_empty() || words.len() > 6 {
        return false;
    }
    let cap = words
        .iter()
        .filter(|w| w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .count();
    let has_sentence_punct = l.ends_with('.') && l.contains(". ");
    cap * 2 >= words.len() && !has_sentence_punct
}

/// Strict JSON the LLM must return. Every field optional; `phones` a list;
/// each field carries an independent confidence in `confidence`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtractedFields {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub phones: Vec<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub socials: Socials,
    /// Per-field 0.0–1.0 confidence; missing key ⇒ treated as 0.
    #[serde(default)]
    pub confidence: std::collections::BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Socials {
    #[serde(default)]
    pub linkedin: Option<String>,
    #[serde(default)]
    pub twitter: Option<String>,
    #[serde(default)]
    pub github: Option<String>,
}

impl ExtractedFields {
    pub fn is_empty(&self) -> bool {
        self.role.is_none()
            && self.title.is_none()
            && self.company.is_none()
            && self.phones.is_empty()
            && self.address.is_none()
            && self.website.is_none()
            && self.socials == Socials::default()
    }

    /// Confidence for `field` (0 if absent).
    pub fn conf(&self, field: &str) -> f64 {
        self.confidence.get(field).copied().unwrap_or(0.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SigError {
    #[error("reasoner: {0}")]
    Reasoner(String),
    #[error("parse: model did not return JSON after retry")]
    Parse,
}

/// Drives the LLM extraction over the shared `Reasoner` seam. Tests inject a
/// fake reasoner; prod uses `ClaudeCliReasoner`.
pub struct SignatureExtractor<'a> {
    pub reasoner: &'a dyn Reasoner,
}

const SYS_PROMPT: &str = "You extract structured contact facts from an email \
signature block. Return ONLY a single JSON object, no prose, no code fence. \
Schema: {\"role\":string|null,\"title\":string|null,\"company\":string|null,\
\"phones\":[string],\"address\":string|null,\"website\":string|null,\
\"socials\":{\"linkedin\":string|null,\"twitter\":string|null,\"github\":string|null},\
\"confidence\":{\"role\":0..1,\"title\":0..1,\"company\":0..1,\"phones\":0..1,\
\"address\":0..1,\"website\":0..1}}. NEVER invent a value that is not present \
in the block — use null. Confidence reflects how certain you are the value \
is correct AND present.";

impl<'a> SignatureExtractor<'a> {
    pub fn new(reasoner: &'a dyn Reasoner) -> Self {
        Self { reasoner }
    }

    /// Extract fields from a detected sig block. One retry on parse failure
    /// (the prompt re-states "JSON only"); then a pure-regex fallback so we
    /// still capture high-confidence phones/URLs without the LLM.
    pub async fn extract(&self, block: &str) -> Result<ExtractedFields, SigError> {
        let opts = ReasonerOpts {
            system_prompt: SYS_PROMPT.to_string(),
            model: None,
            allowed_tools: vec![],
            add_dirs: vec![],
            permission_mode: "default".to_string(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
        };

        for attempt in 0..2 {
            let user = if attempt == 0 {
                format!("Signature block:\n```\n{block}\n```")
            } else {
                format!(
                    "Return ONLY the JSON object (no fence, no prose). \
                     Signature block:\n```\n{block}\n```"
                )
            };
            match self.reasoner.call(&opts, &user).await {
                Ok(out) => {
                    if let Some(parsed) = parse_json_lenient(&out) {
                        return Ok(merge_with_regex(parsed, block));
                    }
                }
                Err(e) => {
                    // Network/spawn failure: don't burn a retry forever.
                    if attempt == 1 {
                        return Err(SigError::Reasoner(e.to_string()));
                    }
                }
            }
        }
        // Fallback: heuristics-only (never empty-invents; only what's literally
        // in the block).
        let rx = regex_only(block);
        if rx.is_empty() {
            Err(SigError::Parse)
        } else {
            Ok(rx)
        }
    }
}

/// Accept raw JSON, a ```json fenced block, or JSON with leading/trailing
/// prose — extract the first balanced `{...}` and parse it.
pub fn parse_json_lenient(s: &str) -> Option<ExtractedFields> {
    let s = s.trim();
    if let Ok(v) = serde_json::from_str::<ExtractedFields>(s) {
        return Some(v);
    }
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s[start..].char_indices() {
        match c {
            '"' if !esc => in_str = !in_str,
            '\\' if in_str => {
                esc = !esc;
                continue;
            }
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    let cand = &s[start..start + i + 1];
                    return serde_json::from_str::<ExtractedFields>(cand).ok();
                }
            }
            _ => {}
        }
        esc = false;
    }
    None
}

/// Fill any field the LLM left null but the regex pass found verbatim in the
/// block — strictly additive (never overrides an LLM value), and only for
/// objectively-present tokens (phones / URLs), so "empty stays empty" holds.
fn merge_with_regex(mut f: ExtractedFields, block: &str) -> ExtractedFields {
    let rx = regex_only(block);
    if f.phones.is_empty() && !rx.phones.is_empty() {
        f.phones = rx.phones;
        f.confidence.entry("phones".into()).or_insert(0.9);
    }
    if f.website.is_none() && rx.website.is_some() {
        f.website = rx.website;
        f.confidence.entry("website".into()).or_insert(0.9);
    }
    if f.socials.linkedin.is_none() {
        f.socials.linkedin = rx.socials.linkedin;
    }
    f
}

/// Pure regex/heuristic extraction of the objectively-present tokens:
/// phone-like digit runs, URLs, a LinkedIn handle. High confidence by
/// construction (they're literally in the text). Never guesses role/company.
pub fn regex_only(block: &str) -> ExtractedFields {
    let mut f = ExtractedFields::default();
    for line in block.lines() {
        let l = line.trim();
        // Phone: a line that is mostly digits + phone punctuation.
        let digits = l.chars().filter(|c| c.is_ascii_digit()).count();
        if (7..=15).contains(&digits)
            && l.chars().all(|c| {
                c.is_ascii_digit() || " +-().x/\t".contains(c)
            })
        {
            let normalized: String = l
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '+')
                .collect();
            if !f.phones.contains(&normalized) {
                f.phones.push(normalized);
            }
        }
        // URL / website.
        for tok in l.split_whitespace() {
            let t = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != ':' && c != '.');
            let low = t.to_ascii_lowercase();
            if low.contains("linkedin.com/in/") || low.contains("linkedin.com/company/") {
                f.socials.linkedin.get_or_insert_with(|| t.to_string());
            } else if (low.starts_with("http://")
                || low.starts_with("https://")
                || low.starts_with("www."))
                && f.website.is_none()
            {
                f.website = Some(t.to_string());
            }
        }
    }
    if !f.phones.is_empty() {
        f.confidence.insert("phones".into(), 0.92);
    }
    if f.website.is_some() {
        f.confidence.insert("website".into(), 0.9);
    }
    f
}

/// Build a fill-blanks wiki patch from extracted fields. Only fields whose
/// confidence ≥ `min_conf` are written; lower-confidence ones are returned
/// separately so the caller can batch them into the daily Discord approval
/// digest (issue: "low-confidence → daily Discord digest").
pub fn signature_patch(
    fields: &ExtractedFields,
    today: &str,
    min_conf: f64,
) -> (PersonPatch, Vec<String>) {
    let mut p = PersonPatch::new()
        .source(format!("Email-signature extraction on {today}"));
    let mut deferred: Vec<String> = Vec::new();

    let consider = |key: &str,
                        label: &str,
                        val: Option<&str>,
                        p: &mut PersonPatch,
                        deferred: &mut Vec<String>| {
        if let Some(v) = val.map(str::trim).filter(|v| !v.is_empty()) {
            if fields.conf(key) >= min_conf {
                *p = std::mem::take(p).profile_row(label, v);
            } else {
                deferred.push(format!("{label}: {v} (conf {:.2})", fields.conf(key)));
            }
        }
    };

    consider("title", "Title", fields.title.as_deref(), &mut p, &mut deferred);
    consider("role", "Role", fields.role.as_deref(), &mut p, &mut deferred);
    consider(
        "company",
        "Company",
        fields.company.as_deref(),
        &mut p,
        &mut deferred,
    );
    consider(
        "address",
        "Address",
        fields.address.as_deref(),
        &mut p,
        &mut deferred,
    );
    consider(
        "website",
        "Website",
        fields.website.as_deref(),
        &mut p,
        &mut deferred,
    );

    if fields.conf("phones") >= min_conf {
        for ph in &fields.phones {
            if !ph.trim().is_empty() {
                p = p.identity("phone", ph.trim());
            }
        }
        if let Some(first) = fields.phones.first() {
            p = p.profile_row("Phone", first.trim());
        }
    } else if !fields.phones.is_empty() {
        deferred.push(format!(
            "Phone: {} (conf {:.2})",
            fields.phones.join(", "),
            fields.conf("phones")
        ));
    }

    if let Some(li) = fields.socials.linkedin.as_deref() {
        // A LinkedIn URL in a sig is objectively present → always safe.
        p = p.profile_row("LinkedIn URL", li);
    }

    (p, deferred)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[test]
    fn strip_quoted_reply_keeps_signature() {
        let raw = "Hey, sounds good.\n\n-- \nJane Doe\nCTO, Acme\n+1 415 555 0100\n\nOn Mon, X wrote:\n> old stuff\n";
        let s = strip_quoted_reply(raw);
        assert!(s.contains("-- "));
        assert!(s.contains("Jane Doe"));
        assert!(!s.contains("old stuff"));
        assert!(!s.contains("On Mon"));
    }

    #[test]
    fn detects_explicit_delimiter_block() {
        let body = "Thanks for the update.\n\n-- \nJane Doe\nCTO at Acme\n+1 415 555 0100\nhttps://acme.com";
        let b = detect_signature_block(body).unwrap();
        assert_eq!(b.reason, "delimiter");
        assert!(b.text.starts_with("Jane Doe"));
        assert!(!b.text.contains("Thanks for the update"));
    }

    #[test]
    fn detects_closing_salutation_block() {
        let body = "Let's sync next week about the roadmap.\n\nBest,\nJane Doe\nVP Engineering, Acme\n+1 (415) 555-0100";
        let b = detect_signature_block(body).unwrap();
        assert_eq!(b.reason, "closing");
        assert!(b.text.contains("Jane Doe"));
    }

    #[test]
    fn detects_mobile_trailer() {
        let body = "yep works for me\n\nSent from my iPhone";
        let b = detect_signature_block(body).unwrap();
        assert!(b.text.to_lowercase().contains("sent from my iphone"));
    }

    #[test]
    fn no_signature_on_plain_prose() {
        let body = "Hi team,\n\nThe deploy went out at noon and metrics look healthy. \
                    I'll keep watching the error rate through the afternoon and report back \
                    if anything regresses. Let me know if you need anything else.";
        assert!(detect_signature_block(body).is_none());
    }

    #[test]
    fn density_block_with_phone() {
        let body = "Approved — ship it.\n\nJane Doe\nAcme Corp\n415-555-0100\njane@acme.com";
        let b = detect_signature_block(body).unwrap();
        assert!(b.text.contains("415-555-0100"));
    }

    #[test]
    fn regex_only_pulls_phone_and_url() {
        let block = "Jane Doe\nCTO\n+1 (415) 555-0100\nhttps://acme.com\nlinkedin.com/in/janedoe";
        let f = regex_only(block);
        assert_eq!(f.phones, vec!["+14155550100"]);
        assert_eq!(f.website.as_deref(), Some("https://acme.com"));
        assert!(f
            .socials
            .linkedin
            .as_deref()
            .unwrap()
            .contains("linkedin.com/in/janedoe"));
        assert!(f.conf("phones") > 0.9);
    }

    #[test]
    fn parse_json_lenient_handles_fence_and_prose() {
        let raw = "Sure! Here is the JSON:\n```json\n{\"company\":\"Acme\",\"phones\":[\"+1415\"],\"confidence\":{\"company\":0.8}}\n```\nHope that helps.";
        let f = parse_json_lenient(raw).unwrap();
        assert_eq!(f.company.as_deref(), Some("Acme"));
        assert_eq!(f.conf("company"), 0.8);
    }

    #[test]
    fn signature_patch_gates_low_confidence() {
        let mut f = ExtractedFields {
            company: Some("Acme".into()),
            title: Some("CTO".into()),
            ..Default::default()
        };
        f.confidence.insert("company".into(), 0.95);
        f.confidence.insert("title".into(), 0.40);
        let (patch, deferred) = signature_patch(&f, "2026-05-18", 0.7);
        assert!(patch.profile.iter().any(|(k, v)| k == "Company" && v == "Acme"));
        assert!(!patch.profile.iter().any(|(k, _)| k == "Title"));
        assert_eq!(deferred.len(), 1);
        assert!(deferred[0].contains("CTO"));
    }

    struct FakeReasoner {
        replies: Mutex<Vec<Result<String, String>>>,
    }
    #[async_trait]
    impl Reasoner for FakeReasoner {
        async fn call(
            &self,
            _opts: &ReasonerOpts,
            _user: &str,
        ) -> anyhow::Result<String> {
            let mut r = self.replies.lock().unwrap();
            match r.remove(0) {
                Ok(s) => Ok(s),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    #[tokio::test]
    async fn extractor_parses_clean_json() {
        let fake = FakeReasoner {
            replies: Mutex::new(vec![Ok(
                "{\"company\":\"Acme\",\"title\":\"CTO\",\"phones\":[],\"confidence\":{\"company\":0.9,\"title\":0.9}}"
                    .into(),
            )]),
        };
        let ex = SignatureExtractor::new(&fake);
        let f = ex.extract("Jane Doe\nCTO at Acme").await.unwrap();
        assert_eq!(f.company.as_deref(), Some("Acme"));
        assert_eq!(f.title.as_deref(), Some("CTO"));
    }

    #[tokio::test]
    async fn extractor_retries_then_regex_fallback() {
        // First reply is junk prose (no JSON); second also junk → fall back
        // to regex which still pulls the phone literally in the block.
        let fake = FakeReasoner {
            replies: Mutex::new(vec![
                Ok("I cannot help with that.".into()),
                Ok("Still no JSON, sorry.".into()),
            ]),
        };
        let ex = SignatureExtractor::new(&fake);
        let f = ex
            .extract("Jane Doe\nCTO\n+1 (415) 555-0100")
            .await
            .unwrap();
        assert_eq!(f.phones, vec!["+14155550100"]);
    }

    #[tokio::test]
    async fn extractor_errors_when_nothing_extractable() {
        let fake = FakeReasoner {
            replies: Mutex::new(vec![
                Ok("nope".into()),
                Ok("still nope".into()),
            ]),
        };
        let ex = SignatureExtractor::new(&fake);
        // Block with no phone/url and unparseable LLM → SigError::Parse.
        assert!(matches!(
            ex.extract("Jane Doe\nSenior Person").await,
            Err(SigError::Parse)
        ));
    }
}
