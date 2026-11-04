//! D6 — every place a card can be posted, and how it is covered: through
//! the `AutonomyBroker` (`Wrapped`), or exempt with a reason. The
//! `all_post_approval_sites_wrapped_or_exempt` test scans the workspace and
//! fails on any site this table does not list.

/// How a card-posting site is covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Calls the broker `serve` builds, which `build_approval_surfaces`
    /// wraps in `AutonomyBroker`.
    Wrapped,
    /// Never gated, for the stated reason.
    Exempt(&'static str),
}

const PA: &str = concat!(".post_", "approval(");
const PAC: &str = concat!(".post_", "approval_card(");
const PAN: &str = concat!(".post_", "approval_noted(");

const W: Coverage = Coverage::Wrapped;
const BELOW: Coverage = Coverage::Exempt(
    "inside the inner broker, below the AutonomyBroker: it only ever runs for a card the decorator already decided",
);
const REPOST: Coverage = Coverage::Exempt(
    "owner-driven repost of an existing card (back to queue / recompose / scheduled send): post_approval_card is forwarded ungated",
);

/// `(file, marker, count, coverage)`. Slack's `approvals/card.rs` posts
/// through the Slack API as one of the `MultiSurfaceBroker`'s surfaces, so
/// it is below the decorator too (and has no `post_approval` site of its
/// own). Owner-initiated `imessage compose` posts its card directly, never
/// through the broker: the owner wrote it (exempt by construction).
pub const EXEMPT_CARD_PATHS: &[(&str, &str, usize, Coverage)] = &[
    // The approval surfaces and their plumbing.
    ("crates/augmentagent-approval-discord/src/lib.rs", PA, 2, Coverage::Exempt(
        "ApprovalBroker default bodies (post_approval_card / post_approval_noted fall back to post_approval) of the inner broker",
    )),
    ("crates/augmentagent-approval-discord/src/nudge.rs", PA, 1, W),
    ("crates/augmentagent-approval-discord/src/surface.rs", PA, 1, W),
    ("crates/augmentagent-approval-discord/src/sync.rs", PA, 1, BELOW),
    ("crates/augmentagent-approval-discord/src/sync.rs", PAC, 1, BELOW),
    ("crates/augmentagent-approval-discord/src/sync.rs", PAN, 1, BELOW),
    // The decorator itself, delegating to the inner broker.
    ("crates/augmentagent-autonomy/src/broker.rs", PA, 2, Coverage::Exempt("the AutonomyBroker's own delegation to the inner broker")),
    ("crates/augmentagent-autonomy/src/broker.rs", PAC, 1, REPOST),
    ("crates/augmentagent-autonomy/src/broker.rs", PAN, 4, Coverage::Exempt("the AutonomyBroker's own noted cards")),
    // C10 (D16): an outreach compose the proactive gate held is carded on
    // the AutonomyBroker's inner broker with the gate's reason.
    ("crates/augmentagent-autonomy/src/outreach.rs", PAN, 1, Coverage::Exempt("the proactive gate's own noted card for a held outreach compose")),
    // Channels: every one is handed serve's (wrapped) broker.
    ("crates/augmentagent-channel-discord-dm/src/channel.rs", PA, 2, W),
    ("crates/augmentagent-channel-email/src/channel.rs", PA, 1, W),
    ("crates/augmentagent-channel-email/src/scheduled.rs", PAC, 1, REPOST),
    ("crates/augmentagent-channel-github/src/channel.rs", PA, 1, W),
    ("crates/augmentagent-channel-imessage/src/reply.rs", PA, 1, W),
    ("crates/augmentagent-channel-instagram/src/channel.rs", PA, 2, W),
    ("crates/augmentagent-channel-linkedin/src/channel.rs", PA, 3, W),
    ("crates/augmentagent-channel-linkedin/src/friend_feed.rs", PA, 1, W),
    ("crates/augmentagent-channel-linkedin/src/invitations.rs", PA, 1, W),
    ("crates/augmentagent-channel-linkedin/src/own_posts.rs", PA, 1, W),
    ("crates/augmentagent-channel-slack/src/channel.rs", PA, 2, W),
    ("crates/augmentagent-channel-socialapi/src/inbound.rs", PA, 1, W),
    ("crates/augmentagent-channel-socialapi/src/own_posts.rs", PA, 1, W),
    ("crates/augmentagent-channel-telegram-bot/src/channel.rs", PA, 1, W),
    ("crates/augmentagent-channel-twitter/src/channel.rs", PA, 1, W),
    ("crates/augmentagent-channel-whatsapp/src/channel.rs", PA, 2, W),
    // The CLI: identity-merge cards ride serve's broker; Back to queue and
    // Recompose repost an existing card.
    ("crates/augmentagent-cli/src/main.rs", PA, 1, W),
    ("crates/augmentagent-cli/src/main.rs", PAC, 2, REPOST),
];

/// The markers that post a card.
// Spelled with concat! so this file never counts as a site itself.
pub const CARD_MARKERS: &[&str] = &[
    concat!(".post_", "approval("),
    concat!(".post_", "approval_card("),
    concat!(".post_", "approval_noted("),
];

/// Count each card-posting marker per file under `root/crates/*/src`,
/// skipping comments, `#[cfg(test)]` items, `*_tests.rs` and `tests/`.
pub fn scan_card_sites(
    root: &std::path::Path,
) -> std::collections::BTreeMap<(String, &'static str), usize> {
    let mut found = std::collections::BTreeMap::new();
    let mut stack = vec![root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if name != "tests" && name != "target" && !name.starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !rel.contains("/src/") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in non_test_lines(&text) {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for marker in CARD_MARKERS {
                    let n = line.matches(marker).count();
                    if n > 0 {
                        *found.entry((rel.clone(), *marker)).or_insert(0) += n;
                    }
                }
            }
        }
    }
    found
}

/// Lines outside `#[cfg(test)]` items (brace-counted; braces inside string
/// literals are rare enough here that a miscount would only show up as a
/// failing inventory test, never a silent pass).
fn non_test_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("#[cfg(test)]") {
            let mut depth = 0i64;
            let mut opened = false;
            i += 1;
            while i < lines.len() {
                let code = lines[i].split("//").next().unwrap_or("");
                for ch in code.chars() {
                    match ch {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                let ends_item =
                    (opened && depth <= 0) || (!opened && code.trim_end().ends_with(';'));
                i += 1;
                if ends_item {
                    break;
                }
            }
            continue;
        }
        out.push(lines[i]);
        i += 1;
    }
    out
}
