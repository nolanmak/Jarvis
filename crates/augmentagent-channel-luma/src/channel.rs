//! Channel struct holding the dry-run entry point and a stub live-publish.
//!
//! The live `publish` returns [`LumaError::NotImplemented`] until the
//! browser-drive layer lands (see `augmentagent-browser-client` + the
//! channel-instagram template). At that point the dry-run plan becomes the
//! script the browser executes, with no other API change.

use crate::composer::{compose_plan, FormFillPlan};
use crate::types::Event;
use crate::validate::{validate, ValidationReport};

#[derive(Debug, thiserror::Error)]
pub enum LumaError {
    #[error("validation failed: {0:?}")]
    Validation(ValidationReport),
    #[error("live publish not implemented yet — use `dry_run` to inspect the plan")]
    NotImplemented,
}

/// Outcome of a dry run.
#[derive(Debug, Clone)]
pub struct PlatformResult {
    pub plan: FormFillPlan,
    pub create_url: String,
}

/// Stateless channel handle. State (cookies, browser session) will be
/// added once the browser-drive layer arrives.
#[derive(Debug, Default, Clone, Copy)]
pub struct LumaChannel;

impl LumaChannel {
    pub fn new() -> Self {
        Self
    }

    /// Validate, then return the form-fill plan that would be executed.
    /// Does not touch the network.
    pub fn dry_run(&self, event: &Event) -> Result<PlatformResult, LumaError> {
        let report = validate(event);
        if report.is_err() {
            return Err(LumaError::Validation(report));
        }
        Ok(PlatformResult {
            plan: compose_plan(event),
            create_url: event.create_url_or_default().to_string(),
        })
    }

    /// Live publish — TODO once browser-drive is wired. Always errors today.
    pub fn publish(&self, _event: &Event) -> Result<String, LumaError> {
        Err(LumaError::NotImplemented)
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
        let ch = LumaChannel::new();
        let r = ch.dry_run(&ev()).expect("ok");
        assert_eq!(r.create_url, "https://lu.ma/create");
        assert!(r.plan.len() > 5);
    }

    #[test]
    fn dry_run_refuses_invalid_event() {
        let ch = LumaChannel::new();
        let mut bad = ev();
        bad.title = String::new();
        let err = ch.dry_run(&bad).unwrap_err();
        assert!(matches!(err, LumaError::Validation(_)));
    }

    #[test]
    fn publish_is_not_implemented_today() {
        let ch = LumaChannel::new();
        let err = ch.publish(&ev()).unwrap_err();
        assert!(matches!(err, LumaError::NotImplemented));
    }
}
