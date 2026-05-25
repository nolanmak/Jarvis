//! On-demand skills registry.
//!
//! Today most reasoner presets `include_str!()` an entire skill prompt into the
//! system prompt (see e.g. `ask_opts` / `triage_opts` / `digest_opts` in
//! [`crate::reasoner`]). That costs tokens on every call even when the skill
//! isn't relevant to the user's actual question.
//!
//! Claude Code ships a native [`Skill`] tool that lets the model load a
//! skill body on demand. This module models the side of that contract that
//! _we_ own: an in-process registry of skills the daemon knows about, plus
//! helpers to build a compact one-line index ("Skills available: X, Y, Z —
//! load via the Skill tool") that the reasoner can drop into a system prompt
//! instead of the full skill body.
//!
//! The registry intentionally stops short of actually serving skill bodies to
//! Claude — that's the Skill tool's job, backed by `~/.claude/skills/<name>/`
//! or a project-level plugin path. The Rust side just needs to know which
//! skill names exist so it can advertise them (and so we can grow file-system
//! discovery later without churning the public surface).
//!
//! Wired in by `reasoner.rs` (see #109 follow-up): the high-variance
//! `ask_opts` preset can call [`SkillRegistry::index_line`] and append the
//! result to its system prompt while keeping `Skill` in `allowed_tools`.
//! Fixed-purpose presets (`triage_opts`, `digest_opts`, `tone_summarize_opts`)
//! stay eager-loaded — the issue body explicitly carves those out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One advertised skill. Holds enough metadata to surface the skill to the
/// LLM via a short index line; the actual body lives in the on-disk skill
/// directory and is loaded by Claude Code's native `Skill` tool when the
/// model decides it's relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// Stable identifier used both in the on-disk path and in the index
    /// line shown to the model. E.g. `"email-triage"`, `"grocery"`.
    pub name: String,
    /// One-line summary the model sees in the advertised index. Keep this
    /// short (target <= 80 chars) so the joined index stays small.
    pub description: String,
    /// Optional on-disk root that holds the skill body. None means the
    /// skill is advertised but not yet materialized on disk — callers can
    /// still surface it in the index for the model to discover.
    pub root: Option<PathBuf>,
}

impl SkillEntry {
    /// Construct a registry entry. `name` and `description` are trimmed; an
    /// empty name is rejected via debug_assert (the registry treats it as a
    /// programmer error, not a runtime failure, since entries are static).
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        let name = name.into().trim().to_string();
        let description = description.into().trim().to_string();
        debug_assert!(!name.is_empty(), "SkillEntry::new() got empty name");
        Self {
            name,
            description,
            root: None,
        }
    }

    /// Attach an on-disk root (e.g. `<repo>/skills/email-triage`). Returns
    /// self for builder-style chaining at registry construction time.
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }
}

/// In-process registry of advertised skills. `BTreeMap` for stable ordering
/// — the index line is part of the prompt and we want byte-identical output
/// given the same set of registered skills (cache-prefix friendly).
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillEntry>,
}

impl SkillRegistry {
    /// Empty registry. Callers usually want [`SkillRegistry::starter`]
    /// instead — it pre-registers the skills we already ship in `skills/`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populated registry covering the starter skills we already have
    /// on disk. Roots are left `None` because the registry is constructed
    /// without filesystem context; callers that want to attach a root
    /// should use [`SkillRegistry::with_skills_dir`] after construction.
    ///
    /// The set here mirrors the directories under `skills/` in the repo
    /// today (`email-triage`, `slack-triage`, `discord-triage`, etc.). If a
    /// skill is added to the on-disk dir but not added here, it simply
    /// won't appear in the advertised index — that's the intentional
    /// trade-off vs. blind filesystem enumeration (descriptions matter
    /// for prompt quality, and we want them curated, not auto-generated).
    pub fn starter() -> Self {
        let mut reg = Self::new();
        for (name, desc) in STARTER_SKILLS {
            reg.register(SkillEntry::new(*name, *desc));
        }
        reg
    }

    /// Attach on-disk roots to all skills whose name matches a subdir under
    /// `skills_dir`. Skills with no matching subdir keep `root = None`.
    /// Useful so a daemon constructed at startup can wire on-disk roots to
    /// the registry without each call site re-doing the path math.
    pub fn with_skills_dir(mut self, skills_dir: &Path) -> Self {
        for entry in self.skills.values_mut() {
            let candidate = skills_dir.join(&entry.name);
            if candidate.is_dir() {
                entry.root = Some(candidate);
            }
        }
        self
    }

    /// Add or replace a skill entry. Returns the previous entry if any so
    /// callers can detect collisions in test setups.
    pub fn register(&mut self, entry: SkillEntry) -> Option<SkillEntry> {
        self.skills.insert(entry.name.clone(), entry)
    }

    /// True if the registry has a skill by this name (case-sensitive,
    /// matches the on-disk dir convention).
    pub fn contains(&self, name: &str) -> bool {
        self.skills.contains_key(name)
    }

    /// All registered skill entries in stable (alphabetical) order. Borrowed
    /// to avoid allocating in the hot path that builds the index line.
    pub fn entries(&self) -> impl Iterator<Item = &SkillEntry> {
        self.skills.values()
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// True if no skills are registered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Build a compact one-line index suitable for splicing into a system
    /// prompt. Format:
    ///
    /// ```text
    /// Skills available (load with the Skill tool when relevant): name — desc; name — desc.
    /// ```
    ///
    /// Returns an empty string when the registry is empty so callers can
    /// safely unconditionally append it to a prompt without producing a
    /// dangling preamble.
    pub fn index_line(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let body = self
            .skills
            .values()
            .map(|s| {
                if s.description.is_empty() {
                    s.name.clone()
                } else {
                    format!("{} — {}", s.name, s.description)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "Skills available (load with the Skill tool when relevant): {body}."
        )
    }
}

/// Starter set of skills the daemon advertises by default. Mirrors the
/// directory layout under `skills/` in the repo today. Each entry is
/// `(name, one_line_description)`; ordering doesn't matter (registry sorts).
const STARTER_SKILLS: &[(&str, &str)] = &[
    (
        "email-triage",
        "Decide reply/skip/flag for inbound emails and draft replies in the user's voice.",
    ),
    (
        "slack-triage",
        "Decide reply/skip/flag for inbound Slack DMs and threads.",
    ),
    (
        "discord-triage",
        "Decide reply/skip/flag for inbound Discord DMs and replies.",
    ),
    (
        "linkedin-triage",
        "Decide reply/skip/flag for inbound LinkedIn messages.",
    ),
    (
        "instagram-triage",
        "Decide reply/skip/flag for inbound Instagram DMs.",
    ),
    (
        "twitter-triage",
        "Decide reply/skip/flag for inbound Twitter/X DMs and mentions.",
    ),
    (
        "whatsapp-triage",
        "Decide reply/skip/flag for inbound WhatsApp messages.",
    ),
    (
        "grocery",
        "Build a weekly grocery order from the user's pantry + preferences.",
    ),
    (
        "draft-archetypes",
        "Reusable reply scaffolds (decline, defer, confirm, intro) for outbound drafts.",
    ),
    (
        "invoice-status",
        "Look up current invoice status, draft entries, and recipient/entity config.",
    ),
    (
        "wiki-search",
        "Search the user's wiki for prior context before answering an ad-hoc question.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn new_registry_is_empty() {
        let reg = SkillRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        // Empty registry must produce an empty index so unconditional splice
        // into a system prompt doesn't leave a dangling preamble line.
        assert_eq!(reg.index_line(), "");
    }

    #[test]
    fn starter_includes_known_skills() {
        let reg = SkillRegistry::starter();
        // Pinning the set we ship today; if you add to STARTER_SKILLS, add
        // an assertion here so the prompt change is intentional.
        assert!(reg.contains("email-triage"));
        assert!(reg.contains("grocery"));
        assert!(reg.contains("wiki-search"));
        assert!(reg.contains("invoice-status"));
        assert!(!reg.is_empty());
    }

    #[test]
    fn register_replaces_existing_by_name() {
        let mut reg = SkillRegistry::new();
        assert!(reg.register(SkillEntry::new("foo", "v1")).is_none());
        let prev = reg.register(SkillEntry::new("foo", "v2"));
        assert_eq!(prev.unwrap().description, "v1");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn index_line_is_stable_and_compact() {
        let mut reg = SkillRegistry::new();
        reg.register(SkillEntry::new("zeta", "last alphabetically"));
        reg.register(SkillEntry::new("alpha", "first alphabetically"));
        let line = reg.index_line();
        // Alphabetical ordering guarantees byte-identical output for the
        // same registered set, which keeps the prompt cache-prefix stable.
        assert!(line.starts_with("Skills available"), "got: {line}");
        let alpha_pos = line.find("alpha").expect("alpha present");
        let zeta_pos = line.find("zeta").expect("zeta present");
        assert!(alpha_pos < zeta_pos, "alphabetical order: {line}");
        assert!(line.ends_with('.'), "trailing period: {line}");
    }

    #[test]
    fn index_line_handles_empty_description() {
        let mut reg = SkillRegistry::new();
        reg.register(SkillEntry::new("bare", ""));
        // Empty description must not produce a dangling em-dash.
        let line = reg.index_line();
        assert!(line.contains("bare"));
        assert!(!line.contains("bare —"), "dangling em-dash: {line}");
    }

    #[test]
    fn with_skills_dir_attaches_roots_for_existing_subdirs() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("email-triage")).unwrap();
        // grocery dir intentionally absent so we can confirm "missing -> None".
        let mut reg = SkillRegistry::new();
        reg.register(SkillEntry::new("email-triage", "x"));
        reg.register(SkillEntry::new("grocery", "y"));
        let reg = reg.with_skills_dir(tmp.path());
        let email = reg
            .entries()
            .find(|s| s.name == "email-triage")
            .expect("email-triage present");
        assert_eq!(email.root, Some(tmp.path().join("email-triage")));
        let grocery = reg
            .entries()
            .find(|s| s.name == "grocery")
            .expect("grocery present");
        assert!(grocery.root.is_none(), "missing dir => None root");
    }

    #[test]
    fn entry_constructor_trims_whitespace() {
        let e = SkillEntry::new("  trimmed  ", "  desc  ");
        assert_eq!(e.name, "trimmed");
        assert_eq!(e.description, "desc");
    }
}
