//! Documented CSS/aria selectors for the Partiful create-event form.
//!
//! Single source of truth for the eventual browser-drive layer and the
//! current dry-run composer. Sourced from the upstream skill notes
//! (`.claude/skills/partiful-create/SKILL.md`) and DOM inspection; both
//! should be re-validated against a live page before promoting the
//! channel out of dry-run.

#[derive(Debug, Clone, Copy)]
pub struct Selectors;

impl Selectors {
    /// Title input. Partiful uses a borderless input announced via its
    /// placeholder.
    pub const TITLE: &'static str = r#"input[placeholder*="Event name" i]"#;

    /// Date trigger (popover-style picker; the wait below covers the
    /// mount delay).
    pub const DATE_TRIGGER: &'static str = r#"button[aria-label*="date" i]"#;

    /// Time input (combined input + popover).
    pub const TIME_INPUT: &'static str = r#"input[aria-label*="time" i]"#;

    /// Location input. Autocomplete; we send literal text.
    pub const LOCATION_INPUT: &'static str = r#"input[placeholder*="Location" i]"#;

    /// Description textarea.
    pub const DESCRIPTION_EDITOR: &'static str = r#"textarea[placeholder*="description" i]"#;

    /// Cover-image upload area — at the TOP of the form, not behind an
    /// "Add cover" CTA. Click opens the file/url picker.
    pub const COVER_AREA: &'static str = r#"[data-testid="cover-image-upload"], [aria-label*="cover image" i]"#;

    /// URL field inside the cover picker.
    pub const COVER_URL_INPUT: &'static str = r#"input[placeholder*="URL" i]"#;

    /// Final submit.
    pub const CREATE_BUTTON: &'static str = r#"button:has-text("Create Event")"#;

    /// Share/invite modal that appears post-publish.
    pub const SHARE_MODAL_CLOSE: &'static str =
        r#"[role="dialog"] button[aria-label*="close" i]"#;

    /// Wait budget for the date picker to mount after clicking
    /// [`Self::DATE_TRIGGER`].
    pub const DATE_PICKER_WAIT_MS: u64 = 1_500;

    /// Success URL pattern — Partiful sends you to `partiful.com/e/<id>`.
    pub const SUCCESS_URL_FRAGMENT: &'static str = "partiful.com/e/";
}
