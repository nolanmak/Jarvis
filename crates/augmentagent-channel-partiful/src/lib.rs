//! Partiful (partiful.com) event-creation channel.
//!
//! Partiful has no usable public posting API — event creation is
//! browser-only. This crate ships the **structure + dry-run path** (#171
//! acceptance): input validation, a form-fill plan builder, and the
//! documented form selectors. The live browser-drive happens via
//! [`augmentagent_browser_client`] in a follow-up once a login profile is
//! provisioned.
//!
//! Modeled on `augmentagent-channel-luma` (sibling browser-event channel)
//! with three differences:
//!
//! 1. **Recurring events are not supported by Partiful.** Validation
//!    refuses payloads marked `recurring`. (The current `Event` shape has
//!    no recurring flag yet; the rule lives in `validate` so the future
//!    field flows through it the moment it's added.)
//! 2. Different create URL (`https://partiful.com/create`) and success
//!    URL fragment (`partiful.com/e/`).
//! 3. Slightly different selectors — the cover-image area is at the top
//!    of the form, not behind an "Add cover" CTA.

pub mod channel;
pub mod composer;
pub mod selectors;
pub mod types;
pub mod validate;

pub use channel::{PartifulChannel, PartifulError, PlatformResult};
pub use composer::{compose_plan, FormFillPlan, FormFillStep};
pub use selectors::Selectors;
pub use types::Event;
pub use validate::{ValidationError, ValidationReport};

/// Channel name used for store routing.
pub const PLATFORM: &str = "partiful";

/// Default URL of the Partiful "create event" page.
pub const DEFAULT_CREATE_URL: &str = "https://partiful.com/create";
