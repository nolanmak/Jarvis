//! Slug helpers for deterministic file naming.

/// Convert an email header-style string to a filesystem-safe slug.
///
/// Extracts the address from forms like `"Name <x@y.com>"` and normalizes
/// `@` → `_at_`, `.` → `_`, lowercasing along the way.
pub fn slug_from_email(raw: &str) -> String {
    let addr = extract_address(raw);
    let lowered = addr.to_lowercase();
    let mut out = String::with_capacity(lowered.len() + 4);
    let mut last_underscore = false;
    for c in lowered.chars() {
        let mapped = match c {
            '@' => {
                out.push_str("_at_");
                last_underscore = true;
                continue;
            }
            'a'..='z' | '0'..='9' => c,
            _ => '_',
        };
        if mapped == '_' && last_underscore {
            continue;
        }
        out.push(mapped);
        last_underscore = mapped == '_';
    }
    // trim leading/trailing underscores
    out.trim_matches('_').to_string()
}

fn extract_address(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('<'), raw.rfind('>')) {
        if open < close {
            return raw[open + 1..close].to_string();
        }
    }
    raw.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_email() {
        assert_eq!(
            slug_from_email("jeremy.doe@example.com"),
            "jeremy_doe_at_example_com"
        );
    }

    #[test]
    fn header_form() {
        assert_eq!(
            slug_from_email("Jeremy Doe <jeremy.doe@example.com>"),
            "jeremy_doe_at_example_com"
        );
    }

    #[test]
    fn plus_addressing() {
        assert_eq!(
            slug_from_email("me+label@gmail.com"),
            "me_label_at_gmail_com"
        );
    }

    #[test]
    fn no_double_underscores() {
        assert_eq!(slug_from_email("a..b@x.com"), "a_b_at_x_com");
    }

    #[test]
    fn empty_safe() {
        assert_eq!(slug_from_email(""), "");
    }
}
