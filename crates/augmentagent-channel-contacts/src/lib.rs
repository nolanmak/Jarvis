//! Google Contacts / CardDAV → phone+address identity index (#62).
//!
//! Closes the cross-platform identity loop: phone numbers, mailing
//! addresses, and real names that live only in the user's address book are
//! pulled in, normalized, and merged into `wiki/people/*.md` under the
//! shared **fill-blanks-only** rule. The E.164 phone index
//! (`identity_phone`) lets message triage resolve an inbound `+1415…` to an
//! existing contact before forking a new page.
//!
//! - [`vcard`] — pure RFC 6350/2426 parser (no IO, unit-tested).
//! - [`phone`] — `phonenumber`-backed E.164 normalizer.
//! - [`source`] — `ContactsSource` trait + the upsert/index engine.
//! - [`google`] — Backend A: Google People API via the Composio Google grant.
//! - [`carddav`] — Backend B: generic CardDAV (Nextcloud/Fastmail/Radicale).
//!   iCloud is an explicit v1 non-goal (see module docs).

pub mod carddav;
pub mod google;
pub mod phone;
pub mod source;
pub mod vcard;

pub use carddav::CardDavSource;
pub use google::GooglePeopleSource;
pub use source::{
    contact_patch, contact_slug, ContactDiff, ContactsError, ContactsPull, ContactsReport,
    ContactsSource, ContactsSyncer,
};
pub use vcard::{parse_vcards, VCard};
