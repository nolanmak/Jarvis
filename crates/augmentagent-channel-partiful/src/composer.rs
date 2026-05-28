//! Build a step-by-step form-fill plan for an event.
//!
//! Pure function. The plan is the canonical artifact returned by
//! [`crate::PartifulChannel::dry_run`] and is what the eventual
//! browser-drive layer will execute.

use serde::{Deserialize, Serialize};

use crate::selectors::Selectors;
use crate::types::Event;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormFillStep {
    Navigate { url: String },
    Wait { ms: u64 },
    Click { selector: String },
    Type { selector: String, text: String },
    UploadCoverFromUrl { url: String },
    Submit,
    AssertSuccessUrl { must_contain: String },
    DismissShareModal,
}

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

pub fn compose_plan(event: &Event) -> FormFillPlan {
    let mut steps = Vec::with_capacity(12);

    steps.push(FormFillStep::Navigate {
        url: event.create_url_or_default().to_string(),
    });

    // Partiful's cover area is at the TOP of the form, so the cover step
    // comes BEFORE the title fields. Skipped when no image is supplied.
    if let Some(url) = event.image_url.as_deref() {
        steps.push(FormFillStep::UploadCoverFromUrl {
            url: url.to_string(),
        });
    }

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
    steps.push(FormFillStep::Type {
        selector: Selectors::DESCRIPTION_EDITOR.into(),
        text: event.description.clone(),
    });

    steps.push(FormFillStep::Submit);
    steps.push(FormFillStep::DismissShareModal);
    steps.push(FormFillStep::AssertSuccessUrl {
        must_contain: Selectors::SUCCESS_URL_FRAGMENT.into(),
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
        assert!(matches!(
            plan.steps.last().unwrap(),
            FormFillStep::AssertSuccessUrl { .. }
        ));
    }

    #[test]
    fn cover_step_comes_before_title_when_present() {
        let mut e = ev();
        e.image_url = Some("https://cdn.example.com/cover.png".into());
        let plan = compose_plan(&e);
        let cover_idx = plan
            .steps
            .iter()
            .position(|s| matches!(s, FormFillStep::UploadCoverFromUrl { .. }))
            .unwrap();
        let title_idx = plan
            .steps
            .iter()
            .position(|s| {
                matches!(s, FormFillStep::Type { selector, .. }
                    if selector.contains("Event name"))
            })
            .unwrap();
        assert!(
            cover_idx < title_idx,
            "Partiful cover is at the top of the form — must precede the title step"
        );
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
    fn success_url_targets_partiful_event_path() {
        let plan = compose_plan(&ev());
        match plan.steps.last().unwrap() {
            FormFillStep::AssertSuccessUrl { must_contain } => {
                assert!(must_contain.contains("partiful.com/e/"));
            }
            _ => unreachable!(),
        }
    }
}
