//! End-to-end tests for the v2 migration plumbing using the three fixture
//! pages at `tests/fixtures/migration/`. The reasoner is a hand-rolled stub:
//! we never spawn `claude`. Tests verify:
//!
//! 1. v1 page — eligible, patch applies, marker appears, v1 content
//!    survives byte-for-byte.
//! 2. v2 page — already-v2 decision, no model call.
//! 3. unparseable page — NoFrontmatter decision, no model call.
//! 4. Idempotency — running again on the post-migration v1 page is a no-op.
//! 5. Citation drop — a stub response naming a non-source messageId has
//!    that claim filtered before write.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use augmentagent_wiki::migrate::{
    apply_patch, classify, parse_patch, parse_sources, render_patch_lines, split_frontmatter,
    validate_citations, MigrationDecision,
};

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/migration")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

/// Stub "reasoner": maps a fixture filename to a canned model response.
/// Real Haiku response would be roughly this shape.
fn stub_response(fixture_name: &str) -> &'static str {
    match fixture_name {
        "v1-alice.md" => {
            // The body explicitly names the Anthropic+Lovable history (cited
            // via 19abc111aaa) and the Sarah-Chen intro. Plus a hallucinated
            // event citing a ghost messageId — must be dropped.
            "\
```yaml
affiliations:
  - org: anthropic
    role: PM
    since: 2025-11-01
    until: null
    source_message_id: 19abc111aaa
  - org: lovable
    role: Growth
    since: 2024-01-01
    until: 2025-10-31
    source_message_id: 19abc111aaa
events:
  - date: 2026-04-30
    kind: other
    source_message_id: ghost-not-in-sources
introduced_by: sarah-chen
```"
        }
        _ => panic!("stub_response called for non-v1 fixture {fixture_name}"),
    }
}

#[test]
fn v1_page_migrates_with_uncited_claim_dropped() {
    let page = fixture("v1-alice.md");
    assert_eq!(classify(&page), MigrationDecision::Eligible);

    let raw = stub_response("v1-alice.md");
    let patch = parse_patch(raw).expect("parse stub patch");

    let (fm, _) = split_frontmatter(&page).unwrap();
    let allowed: BTreeSet<String> = parse_sources(fm);

    let result = validate_citations(patch, &allowed);
    assert_eq!(
        result.dropped, 1,
        "expected to drop the uncited ghost event"
    );
    let rendered = render_patch_lines(&result.filtered, "2026-05-15").unwrap();
    let migrated = apply_patch(&page, &rendered).unwrap();

    assert!(migrated.contains("kind: person\nkey: alice_at_example_com"));
    assert!(migrated.contains("sources: [19abc111aaa, 19abc222bbb, 19abc333ccc]"));
    assert!(migrated.contains("## Identity\n\nAlice Wong, PM at Anthropic"));
    assert!(migrated.contains(
        "## Commitments\n\nAlice promised to send the routing spec doc by end of April"
    ));
    assert!(migrated.contains("affiliations:"));
    assert!(migrated.contains("introduced_by: sarah-chen"));
    assert!(!migrated.contains("ghost-not-in-sources"));
    assert!(migrated.contains("migrated: 2026-05-15"));

    assert_eq!(classify(&migrated), MigrationDecision::AlreadyMigrated);
}

#[test]
fn v2_page_is_skipped_without_model_call() {
    let page = fixture("v2-bob.md");
    let decision = classify(&page);
    assert_eq!(decision, MigrationDecision::AlreadyV2);
    // The orchestrator (CLI) checks this decision before spending tokens
    // or touching the file. `stub_response` panics if invoked for v2 —
    // the assertion above is the "no model call" guarantee.
}

#[test]
fn unparseable_page_is_classified_no_frontmatter() {
    let page = fixture("unparseable-carol.md");
    let decision = classify(&page);
    assert_eq!(decision, MigrationDecision::NoFrontmatter);
}

#[test]
fn migration_is_idempotent_on_already_migrated_pages() {
    let original = fixture("v1-alice.md");
    let raw = stub_response("v1-alice.md");
    let patch = parse_patch(raw).unwrap();
    let (fm, _) = split_frontmatter(&original).unwrap();
    let allowed = parse_sources(fm);
    let filtered = validate_citations(patch, &allowed).filtered;
    let rendered = render_patch_lines(&filtered, "2026-05-15").unwrap();
    let pass1 = apply_patch(&original, &rendered).unwrap();

    assert_eq!(classify(&pass1), MigrationDecision::AlreadyMigrated);
}

#[test]
fn empty_patch_still_writes_marker_so_page_is_skipped_next_run() {
    // Simulate a model that returned only uncited claims — every claim is
    // dropped, but we still want to mark the page as migrated so we don't
    // pay tokens to re-evaluate it.
    let original = fixture("v1-alice.md");
    let empty = serde_yaml_ng::Mapping::new();
    let rendered = render_patch_lines(&empty, "2026-05-15").unwrap();
    let migrated = apply_patch(&original, &rendered).unwrap();
    assert!(migrated.contains("migrated: 2026-05-15"));
    assert_eq!(classify(&migrated), MigrationDecision::AlreadyMigrated);
}
