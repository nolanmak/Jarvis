//! Phone → E.164 normalization (#62).
//!
//! Every `TEL` value from any backend is pushed through here before it
//! becomes a wiki `phone:` identity or an `identity_phone` row, so the
//! reverse-lookup index has one canonical key per number
//! (`+14155551234`) regardless of how the source formatted it
//! (`(415) 555-1234`, `415.555.1234`, `+1 415 555 1234`, …).
//!
//! Backed by the `phonenumber` crate (Google libphonenumber port). Numbers
//! that already carry a `+<country>` prefix parse region-free; bare national
//! numbers need a default region (the user's, from
//! `AUGMENTAGENT_DEFAULT_REGION`, falling back to `US`).

use std::str::FromStr;

use phonenumber::country::Id;

/// Resolve the default region for bare national numbers. Env override
/// `AUGMENTAGENT_DEFAULT_REGION` (ISO-3166 alpha-2, e.g. `GB`); default `US`.
pub fn default_region() -> Id {
    std::env::var("AUGMENTAGENT_DEFAULT_REGION")
        .ok()
        .and_then(|s| Id::from_str(s.trim().to_uppercase().as_str()).ok())
        .unwrap_or(Id::US)
}

/// Normalize a raw phone string to E.164 (`+14155551234`).
///
/// Returns `None` for un-parseable / invalid input — the caller treats that
/// as "no phone identity" (empty stays empty; we never store garbage as a
/// lookup key). `region` is the default for numbers lacking a `+` prefix.
pub fn to_e164(raw: &str, region: Id) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip common vCard `tel:` URI prefix and any `ext`/`;` suffixes the
    // libphonenumber parser would choke on.
    let cleaned = trimmed
        .trim_start_matches("tel:")
        .split(';')
        .next()
        .unwrap_or(trimmed)
        .trim();

    let parsed = if cleaned.starts_with('+') {
        phonenumber::parse(None, cleaned)
    } else {
        phonenumber::parse(Some(region), cleaned)
    }
    .ok()?;

    if !phonenumber::is_valid(&parsed) {
        return None;
    }
    Some(
        parsed
            .format()
            .mode(phonenumber::Mode::E164)
            .to_string(),
    )
}

/// Convenience: normalize with the env/`US` default region.
pub fn normalize(raw: &str) -> Option<String> {
    to_e164(raw, default_region())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_national_to_e164() {
        assert_eq!(
            to_e164("(415) 555-2671", Id::US).as_deref(),
            Some("+14155552671")
        );
        assert_eq!(
            to_e164("415.555.2671", Id::US).as_deref(),
            Some("+14155552671")
        );
    }

    #[test]
    fn international_prefix_is_region_free() {
        assert_eq!(
            to_e164("+44 20 7946 0958", Id::US).as_deref(),
            Some("+442079460958")
        );
    }

    #[test]
    fn strips_tel_uri_and_extension() {
        assert_eq!(
            to_e164("tel:+1-415-555-2671;ext=99", Id::US).as_deref(),
            Some("+14155552671")
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(to_e164("not a phone", Id::US).is_none());
        assert!(to_e164("", Id::US).is_none());
        assert!(to_e164("12", Id::US).is_none());
    }

    #[test]
    fn idempotent_on_already_e164() {
        let once = to_e164("+14155552671", Id::US).unwrap();
        let twice = to_e164(&once, Id::US).unwrap();
        assert_eq!(once, twice);
    }
}
