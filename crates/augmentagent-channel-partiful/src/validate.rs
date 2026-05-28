//! Pure-function input validation for the Partiful event payload.
//!
//! Fails fast so the browser session is never spent on inputs Partiful
//! will reject inside the form.

use chrono::NaiveDate;

use crate::types::{Event, MAX_TITLE_LEN};

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
    #[error("date {0:?} did not parse against known Partiful-accepted formats")]
    UnparseableDate(String),
    #[error("time is empty (must include a timezone, e.g. \"6:00 PM EST\")")]
    EmptyTime,
    #[error("location is empty")]
    EmptyLocation,
    #[error("description is empty")]
    EmptyDescription,
    #[error("image_url {0:?} is not http(s)")]
    InvalidImageUrl(String),
    /// Partiful does not support recurring events. Reserved for when the
    /// shared event shape grows a recurring flag.
    #[error("Partiful does not support recurring events")]
    RecurringNotSupported,
}

/// Validate `event` against the channel's input contract.
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

fn parse_date(s: &str) -> Option<NaiveDate> {
    const FORMATS: &[&str] = &[
        "%B %d, %Y",
        "%b %d, %Y",
        "%Y-%m-%d",
        "%m/%d/%Y",
        "%-m/%-d/%Y",
        "%d %B %Y",
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
            location: "Philly".into(),
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
        assert!(validate(&e)
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
    fn bad_image_url_caught() {
        let mut e = good_event();
        e.image_url = Some("file:///tmp/x.png".into());
        assert!(matches!(
            validate(&e).errors[0],
            ValidationError::InvalidImageUrl(_)
        ));
    }
}
