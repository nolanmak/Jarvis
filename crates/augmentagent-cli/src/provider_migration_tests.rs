//! Shared live migration contracts with synthetic pages and production parsing.
use super::{migration_system_prompt, migration_user_prompt};
use augmentagent_channel_core::reasoner::{wiki_migrate_opts, ClaudeCliReasoner, Reasoner};
use augmentagent_channel_core::codex::CodexCliReasoner;
use augmentagent_wiki::migrate::{apply_patch, classify, parse_patch, parse_sources,
    render_patch_lines, split_frontmatter, validate_citations, MigrationDecision};

async fn migration(provider: &dyn Reasoner) {
    augmentagent_channel_core::state_dir::isolate_for_tests(); // #1048
    let wiki = tempfile::tempdir().unwrap();
    let page = "---\nkind: person\nkey: fixture\nsources: [synthetic-message-101]\n---\n# Fixture\n\nFixture works as a Test Engineer at Example Organization, starting 2026-01-01. (m: synthetic-message-101)\n";
    std::fs::create_dir(wiki.path().join("people")).unwrap();
    let path = wiki.path().join("people/fixture.md");
    std::fs::write(&path, page).unwrap();
    let opts = wiki_migrate_opts(migration_system_prompt(
        include_str!("../../../schema/wiki-skill.md")), wiki.path().into());
    let raw = provider.call(&opts, &migration_user_prompt("fixture", page)).await.unwrap();
    let patch = parse_patch(&raw).unwrap();
    assert!(patch.iter().any(|(key, value)| key.as_str() == Some("affiliations")
        && value.as_sequence().is_some_and(|rows| !rows.is_empty())), "{raw}");
    let (frontmatter, _) = split_frontmatter(page).unwrap();
    let filtered = validate_citations(patch, &parse_sources(frontmatter));
    assert_eq!(filtered.dropped, 0, "migration must cite supplied evidence: {raw}");
    let rendered = render_patch_lines(&filtered.filtered, "2026-01-02").unwrap();
    assert!(rendered.contains("synthetic-message-101"));
    let migrated = apply_patch(page, &rendered).unwrap();
    assert!(migrated.starts_with("---\nkind: person\nkey: fixture\nsources: [synthetic-message-101]\n"));
    assert!(migrated.ends_with(page.split_once("\n---\n").unwrap().1));
    assert_eq!(classify(&migrated), MigrationDecision::AlreadyMigrated);
    assert_eq!(std::fs::read_to_string(path).unwrap(), page, "model must not write migration patches directly");

    let thin = "---\nkind: person\nkey: unknown-fixture\nsources: []\n---\n# Unknown fixture\nNo biographical information is recorded.\n";
    let thin_path = wiki.path().join("people/unknown-fixture.md");
    std::fs::write(&thin_path, thin).unwrap();
    let raw = provider.call(&opts, &migration_user_prompt("unknown-fixture", thin)).await.unwrap();
    assert_eq!(std::fs::read_to_string(thin_path).unwrap(), thin);
    assert!(parse_patch(&raw).unwrap().is_empty(), "must not invent migration facts: {raw}");
}

#[tokio::test]
#[ignore = "requires Codex login; synthetic migration pages only"]
async fn live_codex_migration_contract() { migration(&CodexCliReasoner::openai()).await; }

#[tokio::test]
#[ignore = "requires Claude login; same synthetic migration pages as Codex"]
async fn live_claude_migration_contract() { migration(&ClaudeCliReasoner::new()).await; }
