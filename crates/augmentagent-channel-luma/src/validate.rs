//! Pure-function input validation for the Luma event payload.
//!
//! Fails fast at the channel boundary so the browser session is never
//! wasted on inputs that will reject inside the form (e.g. empty title,
//! 800-char body, unparseable date).

use chrono::NaiveDate;

use crate::types::{Event, MAX_TITLE_LEN};

/// Outcome of `validate`. The browser-drive layer should refuse to
/// submit when `report.is_err()` is true.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub errors: Vec<ValidationError>,
}

impl ValidationReport {
    pub fn is_err(&self) -> bool {
        !self.errors.is_empty()
    }

    fn push(&mut self, e: ValidationError) {
        self.errors.push(e);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("title is empty")]
    EmptyTitle,
    #[error("title is {0} chars, exceeds {MAX_TITLE_LEN}")]
    TitleTooLong(usize),
    #[error("date {0:?} did not parse against known Luma-accepted formats")]
    UnparseableDate(String),
    #[error("time is empty (must include a timezone, e.g. \"6:00 PM EST\")")]
    EmptyTime,
    #[error("location is empty")]
    EmptyLocation,
    #[error("description is empty")]
    EmptyDescription,
    #[error("image_url {0:?} is not http(s)")]
    InvalidImageUrl(String),
}

/// Validate `event` against the channel's input contract.
///
/// Pure: same input → same report. Doesn't touch the network, the browser,
/// or wall-clock time. The date parser tries a small list of human formats
/// before giving up — Luma's own picker accepts anything `chrono::NaiveDate`
/// can read.
pub fn validate(event: &Event) -> ValidationReport {
    let mut report = ValidationReport::default();

    if event.title.trim().is_empty() {
        report.push(ValidationError::EmptyTitle);
    } else if event.title.chars().count() > MAX_TITLE_LEN {
        report.push(ValidationError::TitleTooLong(event.title.chars().count()));
    }

    if parse_date(&event.date).is_none() {
        report.push(ValidationError::UnparseableDate(event.date.clone()));
    }

    if event.time.trim().is_empty() {
        report.push(ValidationError::EmptyTime);
    }

    if event.location.trim().is_empty() {
        report.push(ValidationError::EmptyLocation);
    }

    if event.description.trim().is_empty() {
        report.push(ValidationError::EmptyDescription);
    }

    if let Some(url) = event.image_url.as_deref() {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            report.push(ValidationError::InvalidImageUrl(url.to_string()));
        }
    }

    report
}

/// Try the formats Luma's create page accepts in practice.
fn parse_date(s: &str) -> Option<NaiveDate> {
    const FORMATS: &[&str] = &[
        "%B %d, %Y",    // "January 25, 2026"
        "%b %d, %Y",    // "Jan 25, 2026"
        "%Y-%m-%d",     // "2026-01-25"
        "%m/%d/%Y",     // "01/25/2026"
        "%-m/%-d/%Y",   // "1/25/2026"
        "%d %B %Y",     // "25 January 2026"
    ];
    let s = s.trim();
    for fmt in FORMATS {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_event() -> Event {
        Event {
            title: "Demo party".into(),
            date: "January 25, 2026".into(),
            time: "6:00 PM EST".into(),
            location: "555 Main St, Philadelphia, PA".into(),
            description: "Come hang.".into(),
            image_url: None,
            create_url: None,
        }
    }

    #[test]
    fn good_event_validates() {
        assert!(validate(&good_event()).errors.is_empty());
    }

    #[test]
    fn empty_title_caught() {
        let mut e = good_event();
        e.title = "  ".into();
        assert!(validate(&e).errors.contains(&ValidationError::EmptyTitle));
    }

    #[test]
    fn long_title_caught() {
        let mut e = good_event();
        e.title = "a".repeat(MAX_TITLE_LEN + 1);
        assert!(matches!(
            validate(&e).errors[0],
            ValidationError::TitleTooLong(_)
        ));
    }

    #[test]
    fn unparseable_date_caught() {
        let mut e = good_event();
        e.date = "next thursday-ish".into();
        let report = validate(&e);
        assert!(report
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::UnparseableDate(_))));
    }

    #[test]
    fn iso_date_accepted() {
        let mut e = good_event();
        e.date = "2026-01-25".into();
        assert!(validate(&e).errors.is_empty());
    }

    #[test]
    fn slash_date_accepted() {
        let mut e = good_event();
        e.date = "1/25/2026".into();
        assert!(validate(&e).errors.is_empty());
    }

    #[test]
    fn bad_image_url_caught() {
        let mut e = good_event();
        e.image_url = Some("file:///tmp/x.png".into());
        let report = validate(&e);
        assert!(matches!(
            report.errors[0],
            ValidationError::InvalidImageUrl(_)
        ));
    }

    #[test]
    fn https_image_url_accepted() {
        let mut e = good_event();
        e.image_url = Some("https://cdn.example.com/cover.png".into());
        assert!(validate(&e).errors.is_empty());
    }
}
