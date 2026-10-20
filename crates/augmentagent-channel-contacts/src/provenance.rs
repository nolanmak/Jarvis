//! Contact-provenance gate on outbound bodies (#1451).
//!
//! A draft once attributed a third party's mobile number to the owner — a
//! number the knowledge base stored under an unrelated contact. Prompting
//! cannot make that impossible; a deterministic check on the payload can.
//!
//! Every phone number in an outbound body must be traceable: normalized to
//! E.164, it has to belong to a participant of this message (per the stored
//! identity records), appear in the message being replied to, or be declared
//! by the owner. Anything else — a stranger's stored number, or a number no
//! record claims at all, which is what a fabricated one looks like — refuses
//! the write, the way the #994 register check refuses a mis-cased body. The
//! refusal never echoes the digits: logs are a surface.

use std::path::Path;

use crate::phone;

/// One stored "this E.164 belongs to this person" fact, from whatever sources
/// the caller has (the store's `identity_phone` index, the wiki's
/// `people/*.md` front-matter); [`check`] treats them as a set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhoneRecord {
    pub phone_e164: String,
    pub slug: String,
    pub display_name: Option<String>,
}

impl PhoneRecord {
    /// Slug or display name, case- and separator-insensitively, the way a
    /// `--shared-contact` value names this person.
    fn names(&self, decl: &str) -> bool {
        let want = loose(decl);
        loose(&self.slug) == want
            || self
                .display_name
                .as_deref()
                .is_some_and(|n| loose(n) == want)
    }
}

fn loose(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Both halves of what the gate needs from the wiki, from one scan: every
/// `phone:` identity on the people pages (hand-edited pages never went
/// through CRM ingestion, so they are absent from `identity_phone`), and the
/// slugs of this message's own participants. A record carries no addresses —
/// the store index has none to give — so resolving participants to slugs
/// here is what lets a recipient's own stored number through.
pub fn scan_wiki(wiki_root: &Path, participants: &[String]) -> (Vec<PhoneRecord>, Vec<String>) {
    use augmentagent_wiki::identity::IdentityIndex;
    use augmentagent_wiki::layout::WikiLayout;

    let layout = WikiLayout::new(wiki_root.to_path_buf());
    let Ok(people) = IdentityIndex::build(&layout) else {
        return (Vec::new(), Vec::new());
    };
    let region = phone::default_region();
    let mut records = Vec::new();
    for page in people.pages() {
        for raw in &page.identities.phone {
            if let Some(e164) = phone::to_e164(raw, region) {
                records.push(PhoneRecord {
                    phone_e164: e164,
                    slug: page.slug.clone(),
                    display_name: None,
                });
            }
        }
    }
    let slugs = participants
        .iter()
        .filter_map(|addr| people.lookup("email", addr).map(|p| p.slug.clone()))
        .collect();
    (records, slugs)
}

/// Phone numbers in `text`, normalized to E.164 and deduplicated.
///
/// Deliberately conservative: a candidate is a run of digits and phone
/// punctuation carrying 7..=15 digits that `phonenumber` accepts as a *valid*
/// number for the default region. Order ids, prices and dates fall out on one
/// of those tests.
pub fn candidate_numbers(text: &str) -> Vec<String> {
    const GLUE: [char; 6] = ['+', '-', '.', ' ', '(', ')'];
    let region = phone::default_region();
    let mut out: Vec<String> = Vec::new();
    for run in text.split(|c: char| !(c.is_ascii_digit() || GLUE.contains(&c))) {
        let run = run.trim_matches(|c: char| !c.is_ascii_digit() && c != '+');
        let digits = run.chars().filter(char::is_ascii_digit).count();
        if !(7..=15).contains(&digits) {
            continue;
        }
        if let Some(e164) = phone::to_e164(run, region) {
            if !out.contains(&e164) {
                out.push(e164);
            }
        }
    }
    out
}

/// Decide whether a body may be written. `body_numbers` and
/// `inbound_numbers` are E.164; `participant_slugs` are the people on the
/// message (both from [`candidate_numbers`] / [`scan_wiki`]);
/// `declarations` are `--shared-contact` values — a person's slug / display
/// name, or a number written out. Returns the refusal text, which names
/// stored people by slug only and never echoes the digits.
pub fn check(
    body_numbers: &[String],
    records: &[PhoneRecord],
    participant_slugs: &[String],
    declarations: &[String],
    inbound_numbers: &[String],
) -> Option<String> {
    let region = phone::default_region();
    let declared: Vec<String> =
        declarations.iter().filter_map(|d| phone::to_e164(d, region)).collect();
    let mut offenders: Vec<String> = Vec::new();
    let mut refused = 0usize;
    for number in body_numbers {
        // Quoting the counterparty's own message back at them is not a leak:
        // their signature's number arrived in the thread we're replying to.
        if inbound_numbers.contains(number) || declared.contains(number) {
            continue;
        }
        let owners: Vec<&PhoneRecord> =
            records.iter().filter(|r| &r.phone_e164 == number).collect();
        if owners.is_empty() {
            refused += 1;
            continue;
        }
        if owners.iter().any(|o| {
            participant_slugs.iter().any(|s| loose(s) == loose(&o.slug))
                || declarations.iter().any(|d| o.names(d))
        }) {
            continue;
        }
        refused += 1;
        for o in owners {
            if !offenders.contains(&o.slug) {
                offenders.push(o.slug.clone());
            }
        }
    }
    (refused > 0).then(|| refusal(refused, &offenders))
}

/// The refusal text. Digit-free by construction, and it tells the drafter to
/// REMOVE the number — declaring it is the owner's call, not the agent's.
fn refusal(refused: usize, offenders: &[String]) -> String {
    let stored = if offenders.is_empty() {
        String::new()
    } else {
        format!(
            " Stored as belonging to {}, who is not on this message.",
            offenders.join(", "),
        )
    };
    format!(
        "refusing to write this body (#1451): it contains {refused} phone number(s) this \
         message cannot account for.{stored} Remove them — a number you cannot trace to a \
         stored record of someone on this message, or to the message you are replying to, \
         must be omitted, not guessed. If the owner explicitly asked to share one, re-run \
         with --shared-contact <person-slug-or-number>."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: &str = "+14155550132";

    fn rec(slug: &str) -> PhoneRecord {
        PhoneRecord {
            phone_e164: N.into(),
            slug: slug.into(),
            display_name: None,
        }
    }

    /// `check` on the one number `N`, against `records`.
    fn run(
        records: &[PhoneRecord],
        slugs: &[&str],
        decls: &[&str],
        inbound: &[&str],
    ) -> Option<String> {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        check(
            &[N.to_string()],
            records,
            &owned(slugs),
            &owned(decls),
            &owned(inbound),
        )
    }

    #[test]
    fn extracts_phones_in_any_formatting_and_ignores_other_digit_runs() {
        let nums = candidate_numbers(
            "Carla: (415) 555-0132. Dana +1 415-555-0188, invoice 90210 for $1,234.56 \
             due 2026-10-19, order 12345678901234567890.",
        );
        assert_eq!(nums, vec![N, "+14155550188"]);
        assert!(candidate_numbers("ref 1111111, acct 000-555-0000").is_empty());
    }

    #[test]
    fn a_number_owned_by_a_participant_or_quoted_inbound_passes() {
        let recs = [rec("carla-example")];
        assert_eq!(run(&recs, &["carla-example"], &[], &[]), None);
        assert_eq!(run(&recs, &[], &[], &[N]), None);
    }

    #[test]
    fn a_strangers_number_is_refused_naming_every_holder_and_no_digits() {
        let recs = [rec("carla-example"), rec("dana-example")];
        let msg = run(&recs, &["client-example"], &[], &[]).expect("must refuse");
        for who in ["carla-example", "dana-example"] {
            assert!(msg.contains(who), "{who} must be named: {msg}");
        }
        assert!(candidate_numbers(&msg).is_empty(), "leaked: {msg}");
        assert!(!msg.contains("5550132"), "{msg}");
    }

    #[test]
    fn declaring_the_owner_by_slug_display_name_or_number_lets_it_through() {
        let mut r = rec("carla-example");
        r.display_name = Some("Carla Example".into());
        for decl in ["carla-example", "Carla Example", "carla_example", "(415) 555-0132"] {
            let out = run(std::slice::from_ref(&r), &[], &[decl], &[]);
            assert_eq!(out, None, "declaration {decl:?} must be honored");
        }
    }

    /// The issue's own shape: a number no record claims at all, which is what
    /// a fabricated one looks like. Untraceable PII is omitted, not guessed.
    #[test]
    fn a_number_no_record_claims_is_refused_without_naming_anyone() {
        let msg = run(&[], &[], &[], &[]).expect("must refuse");
        assert!(!msg.contains("belonging to"), "{msg}");
        assert!(candidate_numbers(&msg).is_empty(), "leaked: {msg}");
        assert_eq!(run(&[], &[], &["+1 415 555 0132"], &[]), None);
    }

    #[test]
    fn the_wiki_scan_indexes_hand_written_pages_and_resolves_participants() {
        let dir = tempfile::tempdir().unwrap();
        let people = dir.path().join("people");
        std::fs::create_dir_all(&people).unwrap();
        std::fs::write(
            people.join("carla-example.md"),
            "---\nkind: person\nidentities:\n  email: [carla@example.com]\n  \
             phone: \"+1 415 555 0132\"\n---\n\n# Carla Example\n",
        )
        .unwrap();
        let (records, slugs) = scan_wiki(dir.path(), &["Carla@example.com".into()]);
        assert_eq!(records, vec![rec("carla-example")]);
        assert_eq!(slugs, vec!["carla-example".to_string()]);
    }
}
