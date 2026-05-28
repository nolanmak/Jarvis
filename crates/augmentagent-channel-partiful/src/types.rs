//! Public types for the Partiful event-creation channel.

use serde::{Deserialize, Serialize};

/// Maximum length the title field accepts (mirrors Luma's cap; Partiful
/// has not advertised a hard limit but ~200 is safe).
pub const MAX_TITLE_LEN: usize = 200;

/// A single event payload that maps 1:1 onto the Partiful create form.
///
/// Field shapes mirror the documented inputs in the upstream
/// `.claude/skills/partiful-create/SKILL.md`. The `recurring` flag is
/// kept off the struct on purpose — Partiful doesn't support recurring
/// events. If we add recurring to the shared event shape later,
/// [`crate::validate::validate`] is where the rejection will live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Display title (≤ [`MAX_TITLE_LEN`]).
    pub title: String,
    /// Natural-language date, e.g. `"January 25, 2026"`.
    pub date: String,
    /// Time with timezone, e.g. `"6:00 PM EST"`.
    pub time: String,
    /// Venue name or street address.
    pub location: String,
    /// Body description.
    pub description: String,
    /// Optional cover-image URL. When set, the composer emits a
    /// `UploadCoverFromUrl` step targeting the form's top cover area.
    #[serde(default)]
    pub image_url: Option<String>,
    /// Override the create-page URL. Defaults to
    /// [`crate::DEFAULT_CREATE_URL`] when `None`.
    #[serde(default)]
    pub create_url: Option<String>,
}

impl Event {
    pub fn create_url_or_default(&self) -> &str {
        self.create_url
            .as_deref()
            .unwrap_or(crate::DEFAULT_CREATE_URL)
    }
}
