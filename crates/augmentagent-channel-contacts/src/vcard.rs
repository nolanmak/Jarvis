//! Pure vCard 3.0 / 4.0 parser (#62 Phase 1).
//!
//! Self-contained on purpose: the only fields CRM ingestion cares about are
//! `FN`, `N`, `TEL`, `EMAIL`, `ADR`, `ORG`, `TITLE`, `ROLE`, `BDAY`, `UID`.
//! A focused parser over that subset is auditable, has zero transitive
//! dependencies, and is trivially unit-testable on hand-labeled fixtures —
//! exactly what the issue's Phase 1 calls for. Handles the two structural
//! gotchas every real address-book export hits:
//!
//! 1. **Line folding** (RFC 6350 §3.2): a CRLF followed by a space/tab is a
//!    continuation of the previous logical line.
//! 2. **Property parameters**: `TEL;TYPE=CELL;VALUE=text:+1 415 555 0100` —
//!    the part before the first unescaped `:` is `NAME;PARAM=VAL;…`.
//!
//! No network, no IO. The CardDAV / Google backends feed it raw text.

/// One parsed contact. Every field is best-effort; absent → empty/`None`.
/// Empty fields *stay empty* — the wiki merger never invents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VCard {
    /// `UID` — stable id for delta dedup. May be empty (older exporters).
    pub uid: String,
    /// `FN` formatted name, else assembled from `N`.
    pub full_name: String,
    /// All `TEL` values, raw (un-normalized — caller runs the E.164 pass).
    pub phones: Vec<String>,
    pub emails: Vec<String>,
    /// First `ADR` rendered as a single human line.
    pub address: Option<String>,
    /// `ORG` first component.
    pub organization: Option<String>,
    /// `TITLE` (preferred) or `ROLE`.
    pub title: Option<String>,
    /// `BDAY` verbatim (`--MMDD` / `YYYY-MM-DD` / `YYYYMMDD`).
    pub birthday: Option<String>,
}

impl VCard {
    pub fn is_empty(&self) -> bool {
        self.full_name.is_empty()
            && self.phones.is_empty()
            && self.emails.is_empty()
            && self.address.is_none()
            && self.organization.is_none()
            && self.title.is_none()
    }
}

/// Parse a buffer that may contain one or many `BEGIN:VCARD … END:VCARD`
/// blocks. Unknown properties are ignored; a malformed card never aborts the
/// batch — it yields whatever fields parsed.
pub fn parse_vcards(input: &str) -> Vec<VCard> {
    let unfolded = unfold(input);
    let mut out = Vec::new();
    let mut cur: Option<VCard> = None;
    let mut n_fallback: Option<String> = None;

    for raw_line in unfolded.lines() {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("BEGIN:VCARD") {
            cur = Some(VCard::default());
            n_fallback = None;
            continue;
        }
        if upper.starts_with("END:VCARD") {
            if let Some(mut c) = cur.take() {
                if c.full_name.is_empty() {
                    if let Some(n) = n_fallback.take() {
                        c.full_name = n;
                    }
                }
                if !c.is_empty() {
                    out.push(c);
                }
            }
            continue;
        }
        let Some(card) = cur.as_mut() else { continue };
        let Some((name, params, value)) = split_property(line) else {
            continue;
        };
        match name.as_str() {
            "FN" => card.full_name = unescape(&value),
            "N" => {
                // N: Family;Given;Additional;Prefix;Suffix
                let parts: Vec<&str> = value.split(';').collect();
                let given = parts.get(1).map(|s| s.trim()).unwrap_or("");
                let family = parts.first().map(|s| s.trim()).unwrap_or("");
                let assembled = format!("{given} {family}").trim().to_string();
                if !assembled.is_empty() {
                    n_fallback = Some(unescape(&assembled));
                }
            }
            "TEL" => {
                let v = unescape(&value);
                if !v.trim().is_empty() {
                    card.phones.push(v);
                }
            }
            "EMAIL" => {
                let v = unescape(&value).trim().to_string();
                if !v.is_empty() {
                    card.emails.push(v);
                }
            }
            "ADR" => {
                if card.address.is_none() {
                    let a = render_adr(&value);
                    if !a.is_empty() {
                        card.address = Some(a);
                    }
                }
            }
            "ORG" => {
                if card.organization.is_none() {
                    let org = unescape(value.split(';').next().unwrap_or("").trim());
                    if !org.is_empty() {
                        card.organization = Some(org);
                    }
                }
            }
            "TITLE" => {
                let t = unescape(&value);
                if !t.trim().is_empty() {
                    card.title = Some(t);
                }
            }
            "ROLE" => {
                if card.title.is_none() {
                    let r = unescape(&value);
                    if !r.trim().is_empty() {
                        card.title = Some(r);
                    }
                }
            }
            "BDAY" => {
                let b = value.trim().to_string();
                if !b.is_empty() {
                    card.birthday = Some(b);
                }
            }
            "UID" => {
                card.uid = value
                    .trim()
                    .trim_start_matches("urn:uuid:")
                    .to_string();
            }
            _ => {}
        }
        let _ = params; // parameters parsed but only structural use today
    }
    out
}

/// RFC 6350 §3.2 line unfolding: a line break followed by a single space or
/// HTAB is a continuation. Normalizes CRLF→LF first.
fn unfold(input: &str) -> String {
    let s = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            match chars.peek() {
                Some(' ') | Some('\t') => {
                    chars.next(); // swallow the fold whitespace
                }
                _ => out.push('\n'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split `NAME;PARAM=VAL;…:value` → (NAME upper, params, value). The first
/// `:` inside a quoted parameter value is *not* treated as the separator.
/// Returns `None` if there's no top-level colon. A group prefix
/// (`item1.TEL`) is stripped to the bare property name.
fn split_property(line: &str) -> Option<(String, Vec<String>, String)> {
    let bytes = line.as_bytes();
    let mut in_quote = false;
    let mut colon = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quote = !in_quote,
            b':' if !in_quote => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let head = &line[..colon];
    let value = line[colon + 1..].to_string();
    let mut segs = head.split(';');
    let raw_name = segs.next()?.trim().to_ascii_uppercase();
    let name = raw_name.rsplit('.').next().unwrap_or(&raw_name).to_string();
    let params: Vec<String> = segs.map(|s| s.to_string()).collect();
    Some((name, params, value))
}

/// Render an `ADR` structured value (`PO;Ext;Street;City;Region;Zip;Country`)
/// into a single readable line, skipping empty components.
fn render_adr(value: &str) -> String {
    let parts: Vec<String> = value
        .split(';')
        .map(|p| unescape(p.trim()))
        .collect();
    // Street(2), City(3), Region(4), Zip(5), Country(6).
    let pick = |i: usize| parts.get(i).map(|s| s.as_str()).unwrap_or("").trim();
    let mut segs: Vec<&str> = Vec::new();
    for i in [2usize, 3, 4, 5, 6] {
        let v = pick(i);
        if !v.is_empty() {
            segs.push(v);
        }
    }
    segs.join(", ")
}

/// vCard text-value unescaping: `\n`→newline-as-space, `\,`→`,`, `\;`→`;`,
/// `\\`→`\`. We collapse `\n` to a space so addresses stay single-line.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push(' '),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push(other);
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_v3() {
        let v = "BEGIN:VCARD\nVERSION:3.0\nFN:Jane Doe\nTEL;TYPE=CELL:+1 415 555 0100\nEMAIL:jane@x.com\nEND:VCARD\n";
        let cards = parse_vcards(v);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].full_name, "Jane Doe");
        assert_eq!(cards[0].phones, vec!["+1 415 555 0100"]);
        assert_eq!(cards[0].emails, vec!["jane@x.com"]);
    }

    #[test]
    fn assembles_name_from_n_when_no_fn() {
        let v = "BEGIN:VCARD\nN:Doe;Jane;;;\nTEL:555\nEND:VCARD\n";
        let cards = parse_vcards(v);
        assert_eq!(cards[0].full_name, "Jane Doe");
    }

    #[test]
    fn unfolds_continuation_lines() {
        // ADR split across a folded line.
        let v = "BEGIN:VCARD\nFN:Jane\nADR;TYPE=HOME:;;123 Main St\n ;Anytown;CA;94000;USA\nEND:VCARD\n";
        let cards = parse_vcards(v);
        assert_eq!(
            cards[0].address.as_deref(),
            Some("123 Main St, Anytown, CA, 94000, USA")
        );
    }

    #[test]
    fn handles_multiple_cards_and_multiple_tels() {
        let v = "BEGIN:VCARD\nFN:A\nTEL:111\nTEL:222\nEND:VCARD\nBEGIN:VCARD\nFN:B\nEMAIL:b@b.com\nEND:VCARD\n";
        let cards = parse_vcards(v);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].phones, vec!["111", "222"]);
        assert_eq!(cards[1].full_name, "B");
    }

    #[test]
    fn org_title_role_bday_uid() {
        let v = "BEGIN:VCARD\nFN:Jane\nORG:Acme Inc;Eng\nTITLE:Staff Engineer\nBDAY:--0312\nUID:urn:uuid:abc-123\nEND:VCARD\n";
        let c = &parse_vcards(v)[0];
        assert_eq!(c.organization.as_deref(), Some("Acme Inc"));
        assert_eq!(c.title.as_deref(), Some("Staff Engineer"));
        assert_eq!(c.birthday.as_deref(), Some("--0312"));
        assert_eq!(c.uid, "abc-123");
    }

    #[test]
    fn role_used_only_when_title_absent() {
        let v = "BEGIN:VCARD\nFN:J\nROLE:Advisor\nEND:VCARD\n";
        assert_eq!(parse_vcards(v)[0].title.as_deref(), Some("Advisor"));
        let v2 = "BEGIN:VCARD\nFN:J\nTITLE:CTO\nROLE:Advisor\nEND:VCARD\n";
        assert_eq!(parse_vcards(v2)[0].title.as_deref(), Some("CTO"));
    }

    #[test]
    fn escaped_commas_and_group_prefix() {
        let v = "BEGIN:VCARD\nFN:Smith\\, Jane\nitem1.TEL:+1 555\nEND:VCARD\n";
        let c = &parse_vcards(v)[0];
        assert_eq!(c.full_name, "Smith, Jane");
        assert_eq!(c.phones, vec!["+1 555"]);
    }

    #[test]
    fn empty_card_dropped() {
        let v = "BEGIN:VCARD\nVERSION:4.0\nEND:VCARD\n";
        assert!(parse_vcards(v).is_empty());
    }

    #[test]
    fn quoted_param_colon_not_treated_as_separator() {
        let v = "BEGIN:VCARD\nFN:J\nTEL;TYPE=\"work:main\":+1 999\nEND:VCARD\n";
        assert_eq!(parse_vcards(v)[0].phones, vec!["+1 999"]);
    }
}
