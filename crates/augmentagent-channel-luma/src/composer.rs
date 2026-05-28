//! Build a step-by-step form-fill plan for an event.
//!
//! Pure: `compose_plan(event) -> FormFillPlan` does not touch the browser.
//! The plan is the canonical artifact returned by [`crate::LumaChannel::dry_run`]
//! and is what the eventual browser-drive layer will execute.

use serde::{Deserialize, Serialize};

use crate::selectors::Selectors;
use crate::types::Event;

/// A single browser interaction. The browser-drive layer maps each step
/// to a CDP / Playwright call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormFillStep {
    /// Navigate the tab to `url`.
    Navigate { url: String },
    /// Wait `ms` milliseconds before the next step. Used for the React
    /// date picker mount delay.
    Wait { ms: u64 },
    /// Click the element matching `selector`.
    Click { selector: String },
    /// Focus + type `text` into `selector`.
    Type { selector: String, text: String },
    /// Open cover upload modal and import from URL.
    UploadCoverFromUrl { url: String },
    /// Final form submit.
    Submit,
    /// Assert the current page URL contains `must_contain` and excludes
    /// all of `must_not_contain`. The browser-drive layer fails the run
    /// if the assertion doesn't hold.
    AssertSuccessUrl {
        must_contain: String,
        must_not_contain: Vec<String>,
    },
    /// Dismiss the post-publish share modal if present. No-op if no modal.
    DismissShareModal,
}

/// The full set of steps the browser must execute, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormFillPlan {
    pub steps: Vec<FormFillStep>,
}

impl FormFillPlan {
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Build the plan. Caller is responsible for having validated the event
/// (see [`crate::validate::validate`]).
pub fn compose_plan(event: &Event) -> FormFillPlan {
    let mut steps = Vec::with_capacity(12);

    steps.push(FormFillStep::Navigate {
        url: event.create_url_or_default().to_string(),
    });
    steps.push(FormFillStep::Type {
        selector: Selectors::TITLE.into(),
        text: event.title.clone(),
    });
    steps.push(FormFillStep::Click {
        selector: Selectors::DATE_TRIGGER.into(),
    });
    steps.push(FormFillStep::Wait {
        ms: Selectors::DATE_PICKER_WAIT_MS,
    });
    steps.push(FormFillStep::Type {
        selector: Selectors::DATE_TRIGGER.into(),
        text: event.date.clone(),
    });
    steps.push(FormFillStep::Type {
        selector: Selectors::TIME_INPUT.into(),
        text: event.time.clone(),
    });
    steps.push(FormFillStep::Type {
        selector: Selectors::LOCATION_INPUT.into(),
        text: event.location.clone(),
    });
    steps.push(FormFillStep::Click {
        selector: Selectors::DESCRIPTION_EDITOR.into(),
    });
    steps.push(FormFillStep::Type {
        selector: Selectors::DESCRIPTION_EDITOR.into(),
        text: event.description.clone(),
    });

    if let Some(url) = event.image_url.as_deref() {
        steps.push(FormFillStep::UploadCoverFromUrl {
            url: url.to_string(),
        });
    }

    steps.push(FormFillStep::Submit);
    steps.push(FormFillStep::DismissShareModal);
    steps.push(FormFillStep::AssertSuccessUrl {
        must_contain: Selectors::SUCCESS_URL_FRAGMENT.into(),
        must_not_contain: Selectors::SUCCESS_URL_FORBIDDEN
            .iter()
            .map(|s| s.to_string())
            .collect(),
    });

    FormFillPlan { steps }
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
    fn plan_starts_with_navigate() {
        let plan = compose_plan(&ev());
        assert!(matches!(plan.steps[0], FormFillStep::Navigate { .. }));
    }

    #[test]
    fn plan_ends_with_success_assertion() {
        let plan = compose_plan(&ev());
        let last = plan.steps.last().unwrap();
        assert!(matches!(
            last,
            FormFillStep::AssertSuccessUrl { .. }
        ));
    }

    #[test]
    fn date_picker_wait_is_present() {
        let plan = compose_plan(&ev());
        assert!(plan.steps.iter().any(|s| matches!(s, FormFillStep::Wait { ms } if *ms >= 1000)));
    }

    #[test]
    fn no_cover_step_when_image_url_absent() {
        let plan = compose_plan(&ev());
        assert!(!plan
            .steps
            .iter()
            .any(|s| matches!(s, FormFillStep::UploadCoverFromUrl { .. })));
    }

    #[test]
    fn cover_step_included_when_image_url_set() {
        let mut e = ev();
        e.image_url = Some("https://cdn.example.com/cover.png".into());
        let plan = compose_plan(&e);
        assert!(plan.steps.iter().any(|s| matches!(
            s,
            FormFillStep::UploadCoverFromUrl { url } if url == "https://cdn.example.com/cover.png"
        )));
    }

    #[test]
    fn plan_serializes_to_json() {
        let plan = compose_plan(&ev());
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("navigate"));
        assert!(json.contains("assert_success_url"));
    }
}
