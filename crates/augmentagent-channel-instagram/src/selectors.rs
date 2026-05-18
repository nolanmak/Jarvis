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

/// The "Reel" choice in the Create popover (#76 §3). Same entry as Post but
/// a different sub-item; absent on accounts where the popover collapses.
pub const CREATE_REEL_CHOICE: Target = Target {
    name: "create_reel_choice",
    layers: &[
        sel(SelectorTier::Aria, "[aria-label='Reel']"),
        sel(SelectorTier::Aria, "svg[aria-label='Reel']"),
        sel(SelectorTier::Text, "div[role='menuitem']:has-text('Reel')"),
        sel(SelectorTier::Text, "span:has-text('Reel')"),
    ],
};

/// The "Story" choice in the Create popover (#76 §3). Story has its own
/// composer route (no crop/caption-step parity with feed).
pub const CREATE_STORY_CHOICE: Target = Target {
    name: "create_story_choice",
    layers: &[
        sel(SelectorTier::Aria, "[aria-label='Story']"),
        sel(SelectorTier::Aria, "svg[aria-label='Story']"),
        sel(SelectorTier::Text, "div[role='menuitem']:has-text('Story')"),
        sel(SelectorTier::Text, "span:has-text('Story')"),
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

/// The hidden file input for Reel uploads — `accept` is video-only on the
/// Reel composer (#76 §3). Falls back to the generic input.
pub const VIDEO_FILE_INPUT: Target = Target {
    name: "video_file_input",
    layers: &[
        sel(SelectorTier::Css, "input[type='file'][accept*='video']"),
        sel(SelectorTier::Css, "input[type='file'][accept*='mp4']"),
        sel(SelectorTier::Css, "input[type='file']"),
        sel(SelectorTier::Structural, "form input[type='file']"),
    ],
};

/// The "Open media gallery" / "Add more" affordance in the carousel
/// composer — clicking re-opens the picker so the next `set_input_files`
/// appends slides (#76 §4).
pub const CAROUSEL_ADD_MORE: Target = Target {
    name: "carousel_add_more",
    layers: &[
        sel(SelectorTier::Aria, "[aria-label='Open media gallery']"),
        sel(SelectorTier::Aria, "svg[aria-label='Open media gallery']"),
        sel(SelectorTier::Aria, "[aria-label='Add']"),
        sel(SelectorTier::Text, "div[role='button']:has-text('Add')"),
    ],
};

/// Per-slide thumbnail in the carousel tray. Used to *count* staged slides
/// (`aria-label^="Slide "`) so the composer can assert the upload landed
/// before advancing. Reorder/drag is explicitly out of scope for v1.
pub const CAROUSEL_SLIDE: Target = Target {
    name: "carousel_slide",
    layers: &[
        sel(SelectorTier::Aria, "div[role='button'][aria-label^='Slide ']"),
        sel(SelectorTier::Aria, "[aria-label*='photo'][role='button']"),
        sel(SelectorTier::Structural, "div[role='dialog'] ul li img"),
    ],
};

/// The Reel cover-frame picker trigger ("Select cover" → opens scrubber).
pub const REEL_COVER_TRIGGER: Target = Target {
    name: "reel_cover_trigger",
    layers: &[
        sel(SelectorTier::Aria, "svg[aria-label='Select cover']"),
        sel(SelectorTier::Aria, "[aria-label='Select cover']"),
        sel(SelectorTier::Aria, "[aria-label*='cover' i]"),
        sel(SelectorTier::Text, "div[role='button']:has-text('Cover')"),
    ],
};

/// The Reel cover-frame scrubber thumb (a `role="slider"`). Driven with a
/// synthetic mouse drag — `slider.fill(value)` is ignored by IG (#76 §3).
pub const REEL_COVER_SLIDER: Target = Target {
    name: "reel_cover_slider",
    layers: &[
        sel(SelectorTier::Aria, "div[role='slider']"),
        sel(SelectorTier::Aria, "[role='slider'][aria-label*='cover' i]"),
        sel(SelectorTier::Structural, "div[role='dialog'] input[type='range']"),
    ],
};

/// The Story composer's share/post CTA. Story has its own route — the CTA
/// reads "Share to story" / "Add to story", *not* "Share" (#76 §3).
pub const STORY_SHARE_BUTTON: Target = Target {
    name: "story_share_button",
    layers: &[
        sel(SelectorTier::Aria, "[aria-label='Add to story']"),
        sel(SelectorTier::Aria, "[aria-label='Share to story']"),
        sel(SelectorTier::Text, "div[role='button']:has-text('Add to story')"),
        sel(SelectorTier::Text, "div[role='button']:has-text('Share to story')"),
        sel(SelectorTier::Text, "button:has-text('Share')"),
        sel(SelectorTier::Structural, "div[role='dialog'] div[role='button']:last-of-type"),
    ],
};

/// The hashtag/mention autocomplete dropdown anchored to the caption caret.
/// If still open at Share, IG truncates the caption at the trigger char
/// (#76 §7) — the composer counts this and presses Escape until it's gone.
pub const CAPTION_AUTOCOMPLETE: Target = Target {
    name: "caption_autocomplete",
    layers: &[
        sel(SelectorTier::Aria, "div[role='listbox']"),
        sel(SelectorTier::Aria, "ul[role='listbox']"),
        sel(SelectorTier::Structural, "div[role='dialog'] [role='option']"),
    ],
};

/// The composer dialog itself. Counting this == 0 after Share is the
/// idempotent "post landed" signal (#76 §2.7) — never re-click Share.
pub const COMPOSER_DIALOG: Target = Target {
    name: "composer_dialog",
    layers: &[
        sel(SelectorTier::Aria, "div[role='dialog']"),
        sel(SelectorTier::Structural, "div[role='dialog'][aria-modal='true']"),
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
    CREATE_REEL_CHOICE,
    CREATE_STORY_CHOICE,
    FILE_INPUT,
    VIDEO_FILE_INPUT,
    CAROUSEL_ADD_MORE,
    CAROUSEL_SLIDE,
    REEL_COVER_TRIGGER,
    REEL_COVER_SLIDER,
    STORY_SHARE_BUTTON,
    CAPTION_AUTOCOMPLETE,
    COMPOSER_DIALOG,
    NEXT_BUTTON,
    CAPTION_FIELD,
    SHARE_BUTTON,
];

// =============================================================================
// Registry self-validation (`instagram validate-selectors`, #76)
// =============================================================================
//
// The substantive remaining #76 work — confirming each selector still
// resolves against Instagram's live DOM — is operator-gated (needs a real
// logged-in session) and is NOT attempted here. What *is* fully autonomous
// and worth catching in CI is **registry-shape rot**: a future selector edit
// that drops a target's last text fallback, breaks resilience ordering, leaves
// a non-file selector on a file-input target, duplicates a name, or de-syncs
// `ALL_TARGETS` from the targets the composer actually walks. Those are the
// failure classes that silently make the live UI walk brittle long before an
// operator notices. This module turns the existing scattered `#[cfg(test)]`
// assertions into one reusable pure check the validate binary can run with no
// network, no browser, no cookies — a dry-run shape audit.

/// One registry-shape problem found by [`validate_selector_registry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryDefect {
    /// The offending target's `name` (or `"<registry>"` for global defects).
    pub target: &'static str,
    /// Human-readable description of what is wrong.
    pub detail: String,
}

/// The numeric resilience rank of a tier (lower = stickier / tried first).
fn tier_rank(t: SelectorTier) -> u8 {
    match t {
        SelectorTier::Aria => 0,
        SelectorTier::Css => 1,
        SelectorTier::Text => 2,
        SelectorTier::Structural => 3,
    }
}

/// Targets whose loss of a *text-tier* fallback is load-bearing: each is an
/// approval-gated terminal action or a flow-blocking step where an aria
/// rename with no text backstop would silently strand the composer. Kept
/// explicit (rather than "every target") because aria-only is correct for
/// some probes (e.g. the autocomplete-dropdown *detection* target).
const TEXT_FALLBACK_REQUIRED: &[&str] = &[
    "share_button",
    "story_share_button",
    "next_button",
    "create_reel_choice",
    "create_story_choice",
];

/// Pure, dependency-free audit of the layered selector registry. Returns the
/// list of shape defects (empty == healthy). This is the engine behind the
/// `instagram validate-selectors` dry-run: it proves the registry is
/// *structurally* sound (the part we can verify without Instagram) and is
/// explicit that live-DOM resolution is a separate operator-gated step.
///
/// Checks, per the #76 §5 resilience strategy:
/// 1. every target has ≥1 layer;
/// 2. layers are in non-decreasing resilience order (aria→css→text→struct);
/// 3. file-input targets only ever select `<input type=file>` shaped queries;
/// 4. load-bearing terminal/flow targets keep a text-tier fallback;
/// 5. no duplicate target names;
/// 6. no empty/whitespace selector query strings;
/// 7. the reel/carousel/story surface targets are all registered (a guard
///    against a future refactor silently dropping a surface from the walk).
pub fn validate_selector_registry() -> Vec<RegistryDefect> {
    let mut defects = Vec::new();

    // 5. duplicate names.
    let mut seen = std::collections::BTreeSet::new();
    for t in ALL_TARGETS {
        if !seen.insert(t.name) {
            defects.push(RegistryDefect {
                target: t.name,
                detail: "duplicate target name in ALL_TARGETS".into(),
            });
        }
    }

    for t in ALL_TARGETS {
        // 1. non-empty.
        if t.layers.is_empty() {
            defects.push(RegistryDefect {
                target: t.name,
                detail: "target has zero selector layers".into(),
            });
            continue;
        }

        // 2. resilience ordering.
        for w in t.layers.windows(2) {
            if tier_rank(w[0].tier) > tier_rank(w[1].tier) {
                defects.push(RegistryDefect {
                    target: t.name,
                    detail: format!(
                        "layers out of resilience order: {:?} precedes {:?}",
                        w[0].tier, w[1].tier
                    ),
                });
                break;
            }
        }

        // 6. no blank queries.
        for (i, l) in t.layers.iter().enumerate() {
            if l.query.trim().is_empty() {
                defects.push(RegistryDefect {
                    target: t.name,
                    detail: format!("layer {i} has an empty selector query"),
                });
            }
        }

        // 3. file-input targets must only ever target a file input.
        if t.name == FILE_INPUT.name || t.name == VIDEO_FILE_INPUT.name {
            for l in t.layers {
                if !l.query.contains("file") {
                    defects.push(RegistryDefect {
                        target: t.name,
                        detail: format!(
                            "file-input target has a non-file selector: {:?}",
                            l.query
                        ),
                    });
                }
            }
        }

        // 4. load-bearing targets need a text-tier backstop.
        if TEXT_FALLBACK_REQUIRED.contains(&t.name)
            && !t.layers.iter().any(|l| l.tier == SelectorTier::Text)
        {
            defects.push(RegistryDefect {
                target: t.name,
                detail: "load-bearing target lost its text-tier fallback \
                         (an aria rename would strand the composer)"
                    .into(),
            });
        }
    }

    // 7. every reel/carousel/story surface target still wired into the walk.
    for required in [
        "create_reel_choice",
        "create_story_choice",
        "video_file_input",
        "carousel_add_more",
        "carousel_slide",
        "reel_cover_trigger",
        "reel_cover_slider",
        "story_share_button",
        "caption_autocomplete",
        "composer_dialog",
    ] {
        if !ALL_TARGETS.iter().any(|t| t.name == required) {
            defects.push(RegistryDefect {
                target: "<registry>",
                detail: format!(
                    "reel/carousel/story surface target `{required}` missing \
                     from ALL_TARGETS"
                ),
            });
        }
    }

    defects
}

/// A one-line, machine-greppable summary of the registry audit for the
/// `validate-selectors` CLI / runbook checkbox. `Ok` carries the healthy
/// target count; `Err` carries the defect list rendered for stderr.
pub fn selector_registry_report() -> Result<String, String> {
    let defects = validate_selector_registry();
    if defects.is_empty() {
        Ok(format!(
            "selector registry OK — {} targets, all layers in resilience \
             order, file-input + text-fallback invariants hold (NOTE: \
             live-DOM resolution is operator-gated and NOT checked here)",
            ALL_TARGETS.len()
        ))
    } else {
        let mut s = format!(
            "selector registry FAILED — {} shape defect(s):\n",
            defects.len()
        );
        for d in &defects {
            s.push_str(&format!("  - [{}] {}\n", d.target, d.detail));
        }
        Err(s)
    }
}

/// Best-effort extraction of the Instagram asset-bundle build hash from a
/// page's HTML / script-src list (#76 §5.5: "stamp every selector hit with
/// the build hash so we can answer 'did our selectors break on the X.Y
/// release?'"). IG serves its JS from
/// `https://static.cdninstagram.com/rsrc.php/v3.../<hash>.js`; the hash is
/// the path segment immediately before `.js` on an `rsrc.php` URL.
///
/// Pure + dependency-free so the composer can call it on every session start
/// (feed it `document.documentElement.outerHTML` or the perf-entry list) and
/// stamp it into the structured selector-resolution logs.
pub fn extract_build_hash(html_or_srcs: &str) -> Option<String> {
    // Scan for `rsrc.php` occurrences, then take the last `/`-segment before
    // a `.js` (optionally followed by a query string) on that URL.
    for start in indices_of(html_or_srcs, "rsrc.php") {
        let tail = &html_or_srcs[start..];
        // Bound the URL at the first whitespace / quote / paren.
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')')
            .unwrap_or(tail.len());
        let url = &tail[..end];
        // Strip a query/fragment.
        let path = url.split(['?', '#']).next().unwrap_or(url);
        if let Some(js_at) = path.rfind(".js") {
            let before = &path[..js_at];
            if let Some(slash) = before.rfind('/') {
                let seg = &before[slash + 1..];
                // IG hashes are non-empty, alnum + `_`/`-`, length-bounded.
                if !seg.is_empty()
                    && seg.len() <= 64
                    && seg
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Some(seg.to_string());
                }
            }
        }
    }
    None
}

fn indices_of(haystack: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = haystack[from..].find(needle) {
        out.push(from + i);
        from += i + needle.len();
    }
    out
}

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

    #[test]
    fn reel_carousel_story_targets_registered() {
        for name in [
            "create_reel_choice",
            "create_story_choice",
            "video_file_input",
            "carousel_add_more",
            "carousel_slide",
            "reel_cover_trigger",
            "reel_cover_slider",
            "story_share_button",
            "caption_autocomplete",
            "composer_dialog",
        ] {
            assert!(
                ALL_TARGETS.iter().any(|t| t.name == name),
                "{name} missing from ALL_TARGETS"
            );
        }
    }

    #[test]
    fn video_input_targets_a_file_input() {
        assert!(VIDEO_FILE_INPUT
            .layers
            .iter()
            .all(|s| s.query.contains("file")));
    }

    #[test]
    fn story_share_has_text_fallback() {
        // Story is approval-gated like feed; its CTA must survive an aria
        // rename, so a text-tier layer is mandatory.
        assert!(STORY_SHARE_BUTTON
            .layers
            .iter()
            .any(|s| s.tier == SelectorTier::Text));
    }

    #[test]
    fn build_hash_from_rsrc_url() {
        let html = r#"<script src="https://static.cdninstagram.com/rsrc.php/v3iX9k/yK/l/en_US/AbCdEf12-_gh.js?_nc_x=Ij"></script>"#;
        assert_eq!(
            extract_build_hash(html).as_deref(),
            Some("AbCdEf12-_gh")
        );
    }

    #[test]
    fn build_hash_none_when_absent() {
        assert_eq!(extract_build_hash("<html>no bundle here</html>"), None);
        // A non-rsrc .js must not match (only IG's rsrc.php bundles count).
        assert_eq!(
            extract_build_hash("<script src='https://example.com/app.js'></script>"),
            None
        );
    }

    #[test]
    fn build_hash_picks_first_rsrc_bundle() {
        let srcs = "https://static.cdninstagram.com/rsrc.php/v3/aa/HASHONE.js \
                    https://static.cdninstagram.com/rsrc.php/v3/bb/HASHTWO.js";
        assert_eq!(extract_build_hash(srcs).as_deref(), Some("HASHONE"));
    }

    // --- registry self-validation (validate-selectors dry-run, #76) ---

    #[test]
    fn live_registry_is_shape_clean() {
        // The shipped registry must pass its own audit — this is the
        // assertion the `instagram validate-selectors` dry-run leans on.
        let defects = validate_selector_registry();
        assert!(
            defects.is_empty(),
            "registry shape defects: {defects:#?}"
        );
        assert!(selector_registry_report().is_ok());
    }

    #[test]
    fn report_string_is_operator_legible_and_flags_live_gate() {
        let msg = selector_registry_report().unwrap();
        assert!(msg.contains("selector registry OK"));
        // The operator must not mistake a shape pass for a live-DOM pass.
        assert!(msg.contains("operator-gated"));
        assert!(msg.contains(&ALL_TARGETS.len().to_string()));
    }

    // The defect detectors are exercised against hand-built bad registries so
    // a future real regression in the live registry would be caught the same
    // way (the predicate, not the data, is under test here).

    fn audit_one(t: &'static Target) -> Vec<RegistryDefect> {
        // Mirror the per-target portion of validate_selector_registry for a
        // single synthetic target so each invariant has a focused test.
        let mut d = Vec::new();
        if t.layers.is_empty() {
            d.push(RegistryDefect {
                target: t.name,
                detail: "target has zero selector layers".into(),
            });
            return d;
        }
        for w in t.layers.windows(2) {
            if tier_rank(w[0].tier) > tier_rank(w[1].tier) {
                d.push(RegistryDefect {
                    target: t.name,
                    detail: "layers out of resilience order".into(),
                });
                break;
            }
        }
        d
    }

    #[test]
    fn detects_out_of_order_layers() {
        static BAD: Target = Target {
            name: "bad_order",
            layers: &[
                Selector {
                    tier: SelectorTier::Text,
                    query: "button:has-text('x')",
                },
                Selector {
                    tier: SelectorTier::Aria,
                    query: "[aria-label='x']",
                },
            ],
        };
        let d = audit_one(&BAD);
        assert!(d.iter().any(|x| x.detail.contains("resilience order")));
    }

    #[test]
    fn detects_empty_layers() {
        static EMPTY: Target = Target {
            name: "empty",
            layers: &[],
        };
        assert!(audit_one(&EMPTY)
            .iter()
            .any(|x| x.detail.contains("zero selector layers")));
    }

    #[test]
    fn every_text_fallback_required_target_actually_has_one() {
        // Guards the TEXT_FALLBACK_REQUIRED list itself: if a name in it is
        // typo'd or a target genuinely loses its text layer, the live audit
        // (run above) fails — assert the cross-reference holds today so the
        // list can't silently rot to a no-op.
        for name in TEXT_FALLBACK_REQUIRED {
            let t = ALL_TARGETS
                .iter()
                .find(|t| &t.name == name)
                .unwrap_or_else(|| panic!("{name} not in ALL_TARGETS"));
            assert!(
                t.layers.iter().any(|l| l.tier == SelectorTier::Text),
                "{name} is required to keep a text fallback but has none"
            );
        }
    }

    #[test]
    fn registry_audit_covers_every_walked_target() {
        // ALL_TARGETS must stay in sync with what the composer walks; the
        // surface-presence check inside validate_selector_registry encodes
        // that, so an empty defect list here means the wiring is intact.
        assert!(validate_selector_registry().is_empty());
        // Sanity: the count is the same set the report advertises.
        assert_eq!(ALL_TARGETS.len(), 16);
    }
}
