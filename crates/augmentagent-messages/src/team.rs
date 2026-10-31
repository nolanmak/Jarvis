//! #1698 — the owner's team roster: who may answer a conversation on the
//! owner's behalf.
//!
//! Membership is deterministic. A sender is a teammate only when its
//! canonical handle ([`handles::canonical`]) equals a roster entry, or it is an
//! email address on a listed `@domain`. Display names are never matched, and
//! no identity mapping is consulted: a chat teammate is listed by the exact
//! platform handle they send from (`slack:U…`, `+1555…`, `discord:…`), so an
//! unverified association can never promote a stranger to teammate.
//!
//! Configured only in the local `.env` ([`TeamRoster::ENV`]); this repo is
//! public, so roster values never live in code.

use std::collections::{HashMap, HashSet};

use crate::handles;

/// How a sender relates to the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamClass {
    /// One of the owner's own accounts.
    Owner,
    /// On the roster; `label` is the display label (or the entry itself).
    Teammate { label: String },
    /// Anyone else — an ordinary conversation participant.
    Other,
}

/// The owner's own identities, as canonical handles.
#[derive(Debug, Clone, Default)]
pub struct OwnerIdentities(HashSet<String>);

impl OwnerIdentities {
    /// Build from raw identities (`a@example.com`, `google:a@example.com`,
    /// `Name <a@example.com>`, phone numbers, platform handles).
    pub fn new<I, S>(raw: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let _ = raw;
        todo!()
    }

    pub fn contains(&self, sender: &str) -> bool {
        let _ = sender;
        todo!()
    }
}

/// The parsed roster.
#[derive(Debug, Clone, Default)]
pub struct TeamRoster {
    /// canonical handle → label
    exact: HashMap<String, String>,
    /// lowercase domain (no `@`) → label
    domains: HashMap<String, String>,
    invalid: Vec<String>,
}

impl TeamRoster {
    /// Comma-separated entries; each `addr`, `handle`, `@domain`, optionally
    /// prefixed `Label=`.
    pub const ENV: &'static str = "AUGMENTAGENT_OWNER_TEAM";

    pub fn parse(raw: &str) -> Self {
        let _ = raw;
        todo!()
    }

    pub fn from_env() -> Self {
        Self::parse(&std::env::var(Self::ENV).unwrap_or_default())
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.domains.is_empty()
    }

    /// Entries that could not be parsed; they are ignored.
    pub fn invalid_entries(&self) -> &[String] {
        &self.invalid
    }

    /// `(entry, label)` pairs, sorted, for `augmentagent team list`.
    pub fn entries(&self) -> Vec<(String, String)> {
        todo!()
    }

    /// Owner wins over the roster, so a domain wildcard covering the owner's
    /// own mailbox never makes the owner a teammate.
    pub fn classify(&self, sender: &str, owner: &OwnerIdentities) -> TeamClass {
        let _ = (sender, owner, handles::ME);
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> OwnerIdentities {
        OwnerIdentities::new(["google:owner@example.com", "owner@example.org"])
    }

    fn teammate(label: &str) -> TeamClass {
        TeamClass::Teammate { label: label.into() }
    }

    #[test]
    fn roster_exact_address_is_teammate() {
        let r = TeamRoster::parse("Ana=ana@example.net");
        assert_eq!(r.classify("Ana Example <ana@example.net>", &owner()), teammate("Ana"));
        assert_eq!(r.classify("ben@example.net", &owner()), TeamClass::Other);
    }

    #[test]
    fn roster_domain_wildcard_is_teammate() {
        let r = TeamRoster::parse("@example.net");
        assert_eq!(r.classify("ben@example.net", &owner()), teammate("@example.net"));
        assert_eq!(r.classify("ben@sub.example.net", &owner()), TeamClass::Other);
        assert_eq!(r.classify("ben@example.com", &owner()), TeamClass::Other);
    }

    #[test]
    fn display_name_alone_never_matches() {
        let r = TeamRoster::parse("Ana=ana@example.net");
        assert_eq!(r.classify("\"Ana\" <stranger@example.org>", &owner()), TeamClass::Other);
        assert_eq!(r.classify("Ana", &owner()), TeamClass::Other);
    }

    #[test]
    fn owner_account_beats_domain_wildcard() {
        let r = TeamRoster::parse("@example.com");
        assert_eq!(r.classify("Owner <owner@example.com>", &owner()), TeamClass::Owner);
        assert_eq!(r.classify("me", &owner()), TeamClass::Owner);
        assert_eq!(r.classify("cy@example.com", &owner()), teammate("@example.com"));
    }

    #[test]
    fn case_and_whitespace_insensitive() {
        let r = TeamRoster::parse("  Ana = ANA@Example.NET ,  @EXAMPLE.ORG ");
        assert_eq!(r.classify(" ana@example.net ", &owner()), teammate("Ana"));
        assert_eq!(r.classify("Dee <DEE@example.org>", &owner()), teammate("@example.org"));
    }

    #[test]
    fn name_angle_addr_extracted() {
        let r = TeamRoster::parse("ana@example.net");
        assert_eq!(
            r.classify("Ana Example <ana@example.net>", &owner()),
            teammate("ana@example.net")
        );
    }

    #[test]
    fn chat_identity_matches_only_exact_roster_handle() {
        let r = TeamRoster::parse("Ana=slack:U123,Ben=+1 415 555 0123");
        assert_eq!(r.classify("Ana <slack:U123>", &owner()), teammate("Ana"));
        assert_eq!(r.classify("Ana <slack:U999>", &owner()), TeamClass::Other);
        assert_eq!(r.classify("+14155550123", &owner()), teammate("Ben"));
        // A domain wildcard never matches a non-email handle.
        let d = TeamRoster::parse("@example.net");
        assert_eq!(d.classify("Ana <slack:U123>", &owner()), TeamClass::Other);
    }

    #[test]
    fn invalid_entry_warns_and_is_ignored() {
        let r = TeamRoster::parse("Ana,@nodot,ana@example.net,,=");
        assert_eq!(r.invalid_entries(), ["Ana", "@nodot", "="]);
        assert_eq!(r.entries(), vec![("email:ana@example.net".to_string(), "ana@example.net".to_string())]);
    }

    #[test]
    fn empty_roster_is_inert() {
        let r = TeamRoster::parse("");
        assert!(r.is_empty());
        assert_eq!(r.classify("ana@example.net", &owner()), TeamClass::Other);
        assert_eq!(r.classify("owner@example.org", &owner()), TeamClass::Owner);
    }
}
