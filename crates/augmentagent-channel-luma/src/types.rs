//! Public types for the Luma event-creation channel.

use serde::{Deserialize, Serialize};

/// Maximum length Luma accepts on the event title field.
pub const MAX_TITLE_LEN: usize = 200;

/// A single event payload that maps 1:1 onto the Luma create form.
///
/// Field shapes mirror the documented inputs in the upstream
/// `.claude/skills/luma-create/SKILL.md`. Recurring events ARE supported by
/// Luma but we don't expose them yet — the form path is different and we
/// haven't modelled it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Display title (≤ [`MAX_TITLE_LEN`]).
    pub title: String,
    /// Natural-language date, e.g. `"January 25, 2026"`. Validation parses
    /// this with chrono so the browser agent gets a stable RFC-3339 date.
    pub date: String,
    /// Time with timezone, e.g. `"6:00 PM EST"`.
    pub time: String,
    /// Venue name or street address.
    pub location: String,
    /// Body description. Apostrophes are passed through unchanged — we
    /// don't go through any Rube/Composio env-var serializer that needs
    /// curly-quote escaping.
    pub description: String,
    /// Optional cover-image URL. When set, the composer emits an extra
    /// step to import the cover via the "Add cover" → URL import path.
    #[serde(default)]
    pub image_url: Option<String>,
    /// Override the create-page URL. Defaults to
    /// [`crate::DEFAULT_CREATE_URL`] when `None`.
    #[serde(default)]
    pub create_url: Option<String>,
}

impl Event {
    pub fn create_url_or_default(&self) -> &str {
        self.create_url.as_deref().unwrap_or(crate::DEFAULT_CREATE_URL)
    }
}
