//! Canonical handle strings shared by the message index and people
//! resolution, so a sender seen in a message and an identity on a person page
//! produce the identical key.
//!
//! Format: `<namespace>:<value>`. Namespaces: `phone` (E.164), `email`
//! (lowercased address), `discord` (user id), `whatsapp` (non-phone JID such
//! as `@lid`), `linkedin` (profile URN), `instagram` (numeric id), `raw`
//! (anything unrecognised, trimmed and lowercased). The owner's own messages
//! use the bare handle [`ME`].

use std::str::FromStr;

pub const ME: &str = "me";

/// Default region for bare national phone numbers
/// (`AUGMENTAGENT_DEFAULT_REGION`, ISO-3166 alpha-2; default `US`).
pub fn default_region() -> phonenumber::country::Id {
    std::env::var("AUGMENTAGENT_DEFAULT_REGION")
        .ok()
        .and_then(|s| phonenumber::country::Id::from_str(s.trim().to_uppercase().as_str()).ok())
        .unwrap_or(phonenumber::country::Id::US)
}

/// `+14155550123`-style E.164, or `None` when the input isn't a valid number.
pub fn e164(raw: &str, region: phonenumber::country::Id) -> Option<String> {
    let cleaned = raw.trim().trim_start_matches("tel:");
    if cleaned.is_empty() || !cleaned.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    if cleaned
        .chars()
        .any(|c| !(c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')' | '.')))
    {
        return None;
    }
    let parsed = if cleaned.starts_with('+') {
        phonenumber::parse(None, cleaned)
    } else {
        phonenumber::parse(Some(region), cleaned)
    }
    .ok()?;
    phonenumber::is_valid(&parsed)
        .then(|| parsed.format().mode(phonenumber::Mode::E164).to_string())
}

/// The address inside `Name <addr>`, or the whole string when unbracketed.
fn bracketed(raw: &str) -> Option<&str> {
    let open = raw.rfind('<')?;
    let close = raw[open..].find('>')? + open;
    Some(raw[open + 1..close].trim())
}

fn is_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !s.contains(char::is_whitespace)
}

/// Canonical handle for a WhatsApp JID: number JIDs become phones, others
/// (`@lid`, `@g.us`) stay namespaced as `whatsapp:`.
pub fn whatsapp_jid(jid: &str) -> String {
    let jid = jid.trim().to_ascii_lowercase();
    if let Some(num) = jid.strip_suffix("@s.whatsapp.net") {
        if let Some(p) = e164(&format!("+{num}"), default_region()) {
            return format!("phone:{p}");
        }
    }
    format!("whatsapp:{jid}")
}

/// Canonicalize a free-form sender / identity value. Recognises the tagged
/// forms the channels store (`Name <discord:ID>`, `<linkedin:URN>`,
/// `<socialapi:instagram:ID>`), RFC 5322 addresses, phone numbers, and
/// WhatsApp JIDs; everything else becomes `raw:`.
pub fn canonical(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case(ME) {
        return ME.to_string();
    }
    let inner = bracketed(trimmed).unwrap_or(trimmed);
    if let Some(id) = inner.strip_prefix("discord:") {
        return format!("discord:{}", id.trim());
    }
    if let Some(urn) = inner.strip_prefix("linkedin:") {
        return format!("linkedin:{}", urn.trim());
    }
    if let Some(rest) = inner.strip_prefix("socialapi:") {
        // socialapi:<network>:<id>
        if let Some((network, id)) = rest.split_once(':') {
            return format!("{}:{}", network.trim().to_ascii_lowercase(), id.trim());
        }
    }
    if inner.ends_with("@s.whatsapp.net") || inner.ends_with("@lid") || inner.ends_with("@g.us") {
        return whatsapp_jid(inner);
    }
    if let Some(addr) = inner.strip_prefix("mailto:").or(Some(inner)) {
        if is_email(addr) {
            return format!("email:{}", addr.to_ascii_lowercase());
        }
    }
    if let Some(p) = e164(inner, default_region()) {
        return format!("phone:{p}");
    }
    format!("raw:{}", trimmed.to_lowercase())
}

/// Canonical handle for a wiki identity value of a known platform. Keeps the
/// two sides of people resolution on one code path.
pub fn identity(platform: &str, value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    Some(match platform {
        "email" => format!("email:{}", v.to_ascii_lowercase()),
        "phone" => format!("phone:{}", e164(v, default_region())?),
        "imessage" => canonical(v),
        "whatsapp" => whatsapp_jid(v),
        "discord" => format!("discord:{v}"),
        "linkedin" => format!("linkedin:{v}"),
        "instagram" => format!("instagram:{v}"),
        "slack" => format!("slack:{v}"),
        "twitter" => format!("twitter:{}", v.trim_start_matches('@').to_ascii_lowercase()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones_normalize_to_e164_whatever_the_formatting() {
        for raw in [
            "+1 (415) 555-0123",
            "415.555.0123",
            "(415) 555-0123",
            "+14155550123",
        ] {
            assert_eq!(canonical(raw), "phone:+14155550123", "{raw}");
        }
    }

    #[test]
    fn emails_lowercase_and_drop_display_name() {
        assert_eq!(
            canonical("Jane Doe <Jane.Doe@Example.com>"),
            "email:jane.doe@example.com"
        );
        assert_eq!(canonical("bo@example.org"), "email:bo@example.org");
    }

    #[test]
    fn tagged_platform_ids() {
        assert_eq!(canonical("Alice <discord:123456789>"), "discord:123456789");
        assert_eq!(canonical("me <discord:42>"), "discord:42");
        assert_eq!(
            canonical("Pat <linkedin:urn:li:fsd_profile:ABC>"),
            "linkedin:urn:li:fsd_profile:ABC"
        );
        assert_eq!(canonical("pat <socialapi:instagram:777>"), "instagram:777");
    }

    #[test]
    fn whatsapp_number_jid_is_a_phone_and_lid_stays_namespaced() {
        assert_eq!(
            canonical("14155550123@s.whatsapp.net"),
            "phone:+14155550123"
        );
        assert_eq!(canonical("99887766@lid"), "whatsapp:99887766@lid");
        assert_eq!(canonical("1203630@g.us"), "whatsapp:1203630@g.us");
    }

    #[test]
    fn me_and_unknowns() {
        assert_eq!(canonical(" ME "), ME);
        assert_eq!(canonical("fotw:recorder"), "raw:fotw:recorder");
        assert_eq!(canonical(""), "raw:");
    }

    #[test]
    fn identity_values_canonicalize_like_message_senders() {
        let pairs = [
            (
                identity("email", "Jane@Example.com"),
                canonical("Jane <jane@example.com>"),
            ),
            (
                identity("phone", "+14155550123"),
                canonical("(415) 555-0123"),
            ),
            (
                identity("imessage", "+14155550123"),
                canonical("+14155550123"),
            ),
            (
                identity("imessage", "jane@example.com"),
                canonical("jane@example.com"),
            ),
            (
                identity("whatsapp", "14155550123@s.whatsapp.net"),
                canonical("+14155550123"),
            ),
            (identity("whatsapp", "5555@lid"), canonical("5555@lid")),
            (identity("discord", "123"), canonical("x <discord:123>")),
            (
                identity("linkedin", "urn:li:fsd_profile:Z"),
                canonical("x <linkedin:urn:li:fsd_profile:Z>"),
            ),
        ];
        for (a, b) in pairs {
            assert_eq!(a.as_deref(), Some(b.as_str()));
        }
        assert_eq!(identity("address", "1 Main St"), None);
        assert_eq!(identity("phone", "not a phone"), None);
    }

    #[test]
    fn canonical_never_panics_on_arbitrary_input() {
        let long = "x".repeat(10_000);
        let samples = [
            "<",
            ">",
            "<>",
            "a<b",
            "@",
            "@@",
            "+",
            "tel:",
            "discord:",
            "socialapi:x",
            "\u{0}",
            "名前 <名前@例え.テスト>",
            "<<discord:1>>",
            long.as_str(),
        ];
        for s in samples {
            let _ = canonical(s);
        }
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..2000 {
            let len = (seed % 40) as usize;
            let mut s = String::new();
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                s.push(char::from_u32((seed % 0x3000) as u32).unwrap_or('?'));
            }
            let _ = canonical(&s);
        }
    }
}
