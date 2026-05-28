//! Channel struct holding the dry-run entry point and a stub live-publish.
//!
//! The live `publish` returns [`PartifulError::NotImplemented`] until the
//! browser-drive layer lands.

use crate::composer::{compose_plan, FormFillPlan};
use crate::types::Event;
use crate::validate::{validate, ValidationReport};

#[derive(Debug, thiserror::Error)]
pub enum PartifulError {
    #[error("validation failed: {0:?}")]
    Validation(ValidationReport),
    #[error("live publish not implemented yet — use `dry_run` to inspect the plan")]
    NotImplemented,
}

#[derive(Debug, Clone)]
pub struct PlatformResult {
    pub plan: FormFillPlan,
    pub create_url: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PartifulChannel;

impl PartifulChannel {
    pub fn new() -> Self {
        Self
    }

    pub fn dry_run(&self, event: &Event) -> Result<PlatformResult, PartifulError> {
        let report = validate(event);
        if report.is_err() {
            return Err(PartifulError::Validation(report));
        }
        Ok(PlatformResult {
            plan: compose_plan(event),
            create_url: event.create_url_or_default().to_string(),
        })
    }

    pub fn publish(&self, _event: &Event) -> Result<String, PartifulError> {
        Err(PartifulError::NotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> Event {
        Event {
            title: "demo".into(),
            date: "January 25, 2026".into(),
            time: "6:00 PM EST".into(),
            location: "Philly".into(),
            description: "fun".into(),
            image_url: None,
            create_url: None,
        }
    }

    #[test]
    fn dry_run_returns_plan_for_valid_event() {
        let ch = PartifulChannel::new();
        let r = ch.dry_run(&ev()).expect("ok");
        assert_eq!(r.create_url, "https://partiful.com/create");
        assert!(r.plan.len() > 5);
    }

    #[test]
    fn dry_run_refuses_invalid_event() {
        let ch = PartifulChannel::new();
        let mut bad = ev();
        bad.title = String::new();
        let err = ch.dry_run(&bad).unwrap_err();
        assert!(matches!(err, PartifulError::Validation(_)));
    }

    #[test]
    fn publish_is_not_implemented_today() {
        let ch = PartifulChannel::new();
        let err = ch.publish(&ev()).unwrap_err();
        assert!(matches!(err, PartifulError::NotImplemented));
    }
}
