//! Frontmatter `updated:` maintenance for person pages.
//!
//! The stale-contact engine derives `last_touch` from the `updated:` date
//! (`augmentagent-proactive/src/person.rs`), so a historical import must
//! stamp the *last message date*, never today — and must never regress a
//! page whose `updated:` is already newer (#885).

/// Return the page with `updated:` set to `date` (`YYYY-MM-DD`) iff `date`
/// is later than the current value or the field is absent. `None` = no
/// change needed. The rest of the page is preserved byte-for-byte.
pub fn bump_updated(page: &str, date: &str) -> Option<String> {
    let Some(rest) = page.strip_prefix("---\n") else {
        return None; // no frontmatter — nothing safe to edit
    };
    let end = rest.find("\n---\n")?;
    let fm = &rest[..end];

    for line in fm.lines() {
        if let Some(existing) = line.strip_prefix("updated:") {
            let existing = existing.trim().trim_matches('\'').trim_matches('"');
            if existing >= date {
                return None; // ISO dates compare lexicographically
            }
            let old_line = line;
            let new_line = format!("updated: {date}");
            let new_fm = fm.replacen(old_line, &new_line, 1);
            return Some(format!("---\n{new_fm}{}", &rest[end..]));
        }
    }
    // No updated: field — insert one at the end of the frontmatter.
    Some(format!("---\n{fm}\nupdated: {date}{}", &rest[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "---\nkind: person\nkey: john\nupdated: 2026-05-01\n---\n\n# John\n";

    #[test]
    fn bumps_older_date_forward() {
        let out = bump_updated(PAGE, "2026-08-26").unwrap();
        assert!(out.contains("updated: 2026-08-26"));
        assert!(!out.contains("2026-05-01"));
        assert!(out.ends_with("\n# John\n"));
    }

    #[test]
    fn never_regresses_newer_date() {
        // page was touched after the last text — leave it alone
        assert!(bump_updated(PAGE, "2026-04-01").is_none());
        assert!(bump_updated(PAGE, "2026-05-01").is_none());
    }

    #[test]
    fn inserts_when_absent() {
        let page = "---\nkind: person\nkey: john\n---\n\n# John\n";
        let out = bump_updated(page, "2026-08-26").unwrap();
        assert!(out.contains("kind: person\nkey: john\nupdated: 2026-08-26\n---"));
    }

    #[test]
    fn no_frontmatter_is_untouched() {
        assert!(bump_updated("# John\n", "2026-08-26").is_none());
    }
}
