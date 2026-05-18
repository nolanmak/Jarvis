//! Layered selector registry for the browser-posting path (#50/#76).
//!
//! Instagram's web DOM is obfuscated (hashed class names) and changes
//! frequently. A single brittle CSS selector is a guaranteed future outage.
//! Instead each logical UI target is a *layered* list of [`Selector`]s, tried
//! in resilience order:
//!
//! 1. **Aria** — `aria-label` / `role`. Most stable: Instagram keeps these
//!    for accessibility/screen-reader compliance even across redesigns.
//! 2. **Css** — semantic-ish CSS (`svg[aria-label=...]`, `input[type=file]`).
//! 3. **Text** — visible button text ("Next", "Share"). Locale-fragile but
//!    survives DOM restructures.
//! 4. **Structural** — last-resort positional/structural XPath-ish CSS.
//!
//! The composer walks the layers in order and uses the first that the
//! sidecar can resolve. Keeping every layer in one registry makes the
//! "Instagram changed their DOM again" fix a single-file diff with no logic
//! churn.

/// One resolution strategy for a UI target, tagged by resilience tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorTier {
    Aria,
    Css,
    Text,
    Structural,
}

#[derive(Debug, Clone, Copy)]
pub struct Selector {
    pub tier: SelectorTier,
    /// A CSS selector string the sidecar's `click`/`wait_for`/`get_text`
    /// understand. Text-tier entries use Playwright's `:has-text()` form
    /// (the sidecar resolves it).
    pub query: &'static str,
}

const fn sel(tier: SelectorTier, query: &'static str) -> Selector {
    Selector { tier, query }
}

/// A named logical UI target with its ordered fallback layers.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub name: &'static str,
    pub layers: &'static [Selector],
}

/// The "+ Create / New post" entry point in the left nav / top bar.
pub const CREATE_ENTRY: Target = Target {
    name: "create_entry",
    layers: &[
        sel(SelectorTier::Aria, "svg[aria-label='New post']"),
        sel(SelectorTier::Aria, "a[href='#'][role='link'] svg[aria-label='New post']"),
        sel(SelectorTier::Aria, "[aria-label='New post']"),
        sel(SelectorTier::Text, "span:has-text('Create')"),
        sel(SelectorTier::Structural, "nav a:nth-last-child(3)"),
    ],
};

/// In the post-type popover (when Create offers Post / Reel / Story), the
/// "Post" choice. On accounts without the popover this target is absent and
/// the composer skips it.
pub const CREATE_POST_CHOICE: Target = Target {
    name: "create_post_choice",
    layers: &[
        sel(SelectorTier::Aria, "[aria-label='Post']"),
        sel(SelectorTier::Text, "span:has-text('Post')"),
    ],
};

/// The hidden `<input type=file>` the OS picker would normally drive. We
/// never open the picker — CDP `setInputFiles` injects the path directly.
pub const FILE_INPUT: Target = Target {
    name: "file_input",
    layers: &[
        sel(SelectorTier::Css, "input[type='file'][accept*='image']"),
        sel(SelectorTier::Css, "input[type='file']"),
        sel(SelectorTier::Structural, "form input[type='file']"),
    ],
};

/// The "Next" button advancing crop → filter → caption (clicked twice).
pub const NEXT_BUTTON: Target = Target {
    name: "next_button",
    layers: &[
        sel(SelectorTier::Aria, "[aria-label='Next']"),
        sel(SelectorTier::Text, "div[role='button']:has-text('Next')"),
        sel(SelectorTier::Text, "button:has-text('Next')"),
        sel(SelectorTier::Structural, "div[role='dialog'] div[role='button']:last-of-type"),
    ],
};

/// The caption contenteditable on the final compose step.
pub const CAPTION_FIELD: Target = Target {
    name: "caption_field",
    layers: &[
        sel(SelectorTier::Aria, "div[aria-label='Write a caption...']"),
        sel(SelectorTier::Aria, "textarea[aria-label='Write a caption...']"),
        sel(SelectorTier::Css, "div[contenteditable='true'][role='textbox']"),
        sel(SelectorTier::Structural, "div[role='dialog'] div[contenteditable='true']"),
    ],
};

/// The final "Share" button. The composer NEVER clicks this without an
/// approval gate (#50).
pub const SHARE_BUTTON: Target = Target {
    name: "share_button",
    layers: &[
        sel(SelectorTier::Aria, "[aria-label='Share']"),
        sel(SelectorTier::Text, "div[role='button']:has-text('Share')"),
        sel(SelectorTier::Text, "button:has-text('Share')"),
        sel(SelectorTier::Structural, "div[role='dialog'] div[role='button']:last-of-type"),
    ],
};

/// Every target the composer touches, in flow order. Used by the
/// registry-completeness test + as a single iteration point.
pub const ALL_TARGETS: &[Target] = &[
    CREATE_ENTRY,
    CREATE_POST_CHOICE,
    FILE_INPUT,
    NEXT_BUTTON,
    CAPTION_FIELD,
    SHARE_BUTTON,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_has_layers_in_resilience_order() {
        for t in ALL_TARGETS {
            assert!(!t.layers.is_empty(), "{} has no layers", t.name);
            // Tiers must be non-decreasing (aria < css < text < structural)
            // so the composer always tries the most-stable strategy first.
            let rank = |s: &Selector| match s.tier {
                SelectorTier::Aria => 0,
                SelectorTier::Css => 1,
                SelectorTier::Text => 2,
                SelectorTier::Structural => 3,
            };
            for w in t.layers.windows(2) {
                assert!(
                    rank(&w[0]) <= rank(&w[1]),
                    "{}: layers not in resilience order",
                    t.name
                );
            }
        }
    }

    #[test]
    fn share_button_has_a_text_fallback() {
        // The Share gate is load-bearing; it must survive an aria rename.
        assert!(SHARE_BUTTON
            .layers
            .iter()
            .any(|s| s.tier == SelectorTier::Text));
    }

    #[test]
    fn file_input_targets_a_file_input() {
        assert!(FILE_INPUT
            .layers
            .iter()
            .all(|s| s.query.contains("file")));
    }

    #[test]
    fn target_names_unique() {
        let mut names: Vec<&str> = ALL_TARGETS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "duplicate target name");
    }
}
