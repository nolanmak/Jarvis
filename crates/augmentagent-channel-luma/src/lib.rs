//! Luma (lu.ma) event-creation channel.
//!
//! Luma has no usable public posting API — event creation is browser-only.
//! This crate ships the **structure + dry-run path** (#170 acceptance):
//! input validation, a form-fill plan builder, and the documented form
//! selectors. The live browser-drive happens via
//! [`augmentagent_browser_client`] in a follow-up once a login profile is
//! provisioned.
//!
//! Modeled on `augmentagent-channel-instagram` (browser-automated channel
//! template) — same layout: `types`/`validate`/`selectors`/`composer`/
//! `channel` — but minimal until the auth + browser-drive layers land.

pub mod channel;
pub mod composer;
pub mod selectors;
pub mod types;
pub mod validate;

pub use channel::{LumaChannel, LumaError, PlatformResult};
pub use composer::{compose_plan, FormFillPlan, FormFillStep};
pub use selectors::Selectors;
pub use types::Event;
pub use validate::{ValidationError, ValidationReport};

/// Channel name used for store routing.
pub const PLATFORM: &str = "luma";

/// Default URL of the Luma "create event" page. Overridable in
/// [`types::Event::create_url`] for staging / l10n routes.
pub const DEFAULT_CREATE_URL: &str = "https://lu.ma/create";
