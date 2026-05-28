//! Documented CSS/aria selectors for the Luma create-event form.
//!
//! Centralized so the browser-drive layer (added later) and the dry-run
//! composer share one source of truth. When Luma ships a UI change, only
//! this file moves.
//!
//! Selectors are sourced from the upstream skill notes
//! (`.claude/skills/luma-create/SKILL.md`) and from direct DOM inspection;
//! both should be re-validated against a live page before promoting the
//! channel out of dry-run.

#[derive(Debug, Clone, Copy)]
pub struct Selectors;

impl Selectors {
    /// Title input — Luma uses a styled textarea that announces itself as
    /// the event title via `placeholder="Event Name"`.
    pub const TITLE: &'static str = r#"textarea[placeholder*="Event Name" i]"#;

    /// Start-date trigger: opens the React date picker. Luma uses a button
    /// labelled with the current selected date in aria-label.
    pub const DATE_TRIGGER: &'static str = r#"[aria-label*="event start date" i]"#;

    /// Start-time input. Combo input with manual entry + popover.
    pub const TIME_INPUT: &'static str = r#"input[placeholder*="time" i]"#;

    /// Location input — autocompletes against Google Places. We send literal
    /// text and accept whatever Luma binds.
    pub const LOCATION_INPUT: &'static str = r#"input[placeholder*="Add Event Location" i]"#;

    /// Description textarea (rich-text contenteditable; the browser agent
    /// must `.click()` it before typing to avoid the placeholder eating the
    /// first keystroke).
    pub const DESCRIPTION_EDITOR: &'static str = r#"[contenteditable="true"][data-placeholder*="Describe" i]"#;

    /// "Add cover" CTA. Only present until a cover has been uploaded.
    pub const COVER_ADD_BUTTON: &'static str = r#"button:has-text("Add Cover")"#;

    /// Within the cover modal, the "From URL" tab.
    pub const COVER_URL_TAB: &'static str = r#"button:has-text("From URL")"#;

    /// URL field inside the cover-from-URL flow.
    pub const COVER_URL_INPUT: &'static str = r#"input[placeholder*="URL" i]"#;

    /// Final submit. Luma uses a styled `<button>` not `<input type=submit>`.
    pub const CREATE_BUTTON: &'static str = r#"button:has-text("Create Event")"#;

    /// Share modal that appears post-publish. The browser agent dismisses
    /// it before checking the success URL.
    pub const SHARE_MODAL_CLOSE: &'static str = r#"[role="dialog"] button[aria-label*="close" i]"#;

    /// Wait budget for the React date picker to render after clicking
    /// [`Self::DATE_TRIGGER`]. Empirically ~2s on a warm session.
    pub const DATE_PICKER_WAIT_MS: u64 = 2_000;

    /// Success URL pattern. After publish, the browser is at `lu.ma/<slug>`
    /// (not `/create` or `/home`). The browser-drive layer asserts this.
    pub const SUCCESS_URL_FRAGMENT: &'static str = "lu.ma/";
    pub const SUCCESS_URL_FORBIDDEN: &'static [&'static str] = &["/create", "/home"];
}
