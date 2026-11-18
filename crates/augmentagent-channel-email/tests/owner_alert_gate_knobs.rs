use augmentagent_store::{owner_alerts::Urgency, Email, Store};

fn mail(body: &str) -> Email {
    Email {
        message_id: "gate-mail".into(),
        thread_id: Some("thread".into()),
        from: "Matt <matt@example.test>".into(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        subject: "Update".into(),
        body: body.into(),
        date: "2026-11-18T09:00:00Z".into(),
        account_entity_id: Some("account".into()),
        platform: "gmail".into(),
        kind: "dm".into(),
    }
}

#[test]
fn owner_priority_needs_no_model_request_or_consequence_evidence() {
    use augmentagent_channel_email::owner_alert_gate::{plan, Gate};
    let result = plan(
        &mail("Just keeping you posted."),
        &Gate::Priority {
            selector: "matt@example.test".into(),
            level: Urgency::High,
        },
        1,
    )
    .unwrap()
    .unwrap();
    assert_eq!(result.urgency, Urgency::High);
    assert!(result.due_at_ms.is_none());
}

#[test]
fn priority_configuration_accepts_a_domain_without_rebuilding() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    store
        .set_owner_alert_priority("example.test", Urgency::High)
        .expect("domain priority selector");
}

#[test]
fn priority_configuration_accepts_a_person_slug_without_rebuilding() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    store
        .set_owner_alert_priority("person:matt", Urgency::High)
        .expect("wiki person priority selector");
}

#[test]
fn all_person_identities_match_and_exact_mute_beats_wider_priority() {
    use augmentagent_channel_email::owner_alert_gate::{classify, Catalog, Gate};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("people")).unwrap();
    std::fs::write(dir.path().join("people/matt.md"),"---\nidentities:\n  email: [matt@example.test, personal@other.test]\nrelationship: close-contact\n---\n").unwrap();
    let catalog = Catalog::load(Some(dir.path())).unwrap();
    let priorities = vec![("person:matt".into(), Urgency::High)];
    for address in ["matt@example.test", "personal@other.test"] {
        assert!(matches!(
            classify(address, "FYI", &priorities, &catalog),
            Gate::Priority {
                level: Urgency::High,
                ..
            }
        ));
    }
    let mut priorities = priorities;
    priorities.push(("matt@example.test".into(), Urgency::Routine));
    assert!(matches!(
        classify("matt@example.test", "FYI", &priorities, &catalog),
        Gate::Muted
    ));
    assert!(matches!(
        classify(
            "stranger@sub.example.test",
            "FYI",
            &[("domain:example.test".into(), Urgency::High)],
            &catalog
        ),
        Gate::Legacy
    ));
}

#[test]
fn documented_request_is_high_with_relationship_evidence_unknown_is_not() {
    use augmentagent_channel_email::owner_alert_gate::{classify, plan, Catalog, Gate};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("people")).unwrap();
    for relation in ["close-contact", "active-collaborator"] {
        std::fs::write(
            dir.path().join("people/matt.md"),
            format!(
                "---\nidentities:\n  email: matt@example.test\nrelationship: {relation}\n---\n"
            ),
        )
        .unwrap();
        let catalog = Catalog::load(Some(dir.path())).unwrap();
        let mail = mail("can you send me the deck?");
        let gate = classify(&mail.from, &mail.body, &[], &catalog);
        let p = plan(&mail, &gate, 1).unwrap().unwrap();
        assert_eq!(p.urgency, Urgency::High);
        assert!(p.evidence.contains(&format!("relationship: {relation}")));
        assert!(p.evidence.contains("can you send me the deck?"));
        assert!(matches!(
            classify("unknown@example.test", &mail.body, &[], &catalog),
            Gate::Legacy
        ));
        assert!(matches!(
            classify(&mail.from, "Can you believe this news?", &[], &catalog),
            Gate::Legacy
        ));
    }
}

#[test]
fn wiki_prose_relationship_declarations_qualify_but_negated_or_historical_ones_do_not() {
    use augmentagent_channel_email::owner_alert_gate::{classify, Catalog, Gate};
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("people")).unwrap();
    for (line, qualifies) in [
        ("Active collaborator on agent projects.", true),
        (
            "GitHub user example; full name Matt. Active collaborator on the repository.",
            true,
        ),
        ("Close contact from the local community.", true),
        ("No longer an active collaborator.", false),
        ("Former close contact.", false),
        ("May become an active collaborator.", false),
        ("Not a close contact.", false),
    ] {
        std::fs::write(
            root.path().join("people/matt.md"),
            format!("---\nidentities:\n  email: matt@example.test\n---\n{line}\n"),
        )
        .unwrap();
        let gate = classify(
            "matt@example.test",
            "can you send me the deck?",
            &[],
            &Catalog::load(Some(root.path())).unwrap(),
        );
        assert_eq!(
            matches!(gate, Gate::Relationship { .. }),
            qualifies,
            "{line}"
        );
    }
}

#[test]
fn arrival_is_opt_in_durable_and_preserves_relationship_evidence_through_assessment() {
    if std::env::var("L5_GATE_CHILD").is_ok() {
        use augmentagent_channel_email::{
            owner_alert_gate::arrival, owner_alerts::persist_assessment_with_wiki,
        };
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(wiki.join("people")).unwrap();
        std::fs::write(wiki.join("people/matt.md"),"---\nidentities:\n  email: [matt@example.test, second@other.test]\nrelationship: close-contact\n---\n").unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let email = mail("can you send me the deck?");
        let now = chrono::Utc::now().timestamp_millis();
        let id = arrival(&store, &email, Some(&wiki), now).unwrap();
        if !augmentagent_channel_email::owner_alert_gate::enabled() {
            assert!(id.is_none());
            return;
        }
        let id = id.unwrap();
        arrival(&store, &email, Some(&wiki), now + 1).unwrap();
        persist_assessment_with_wiki(&store,&email,r#"{"decision":"skip","alert":{"action":"Send me the deck","request_evidence":"can you send me the deck?","reason":"Request"}}"#,Some(&wiki),now+2).unwrap();
        let details: String = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT payload FROM owner_alert_details WHERE alert_id=?1",
                    [&id],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert!(details.contains("relationship: close-contact"));
        assert_eq!(store.owner_alert(&id).unwrap().unwrap().created_at_ms, now);
        return;
    }
    for enabled in [false, true] {
        let mut c = std::process::Command::new(std::env::current_exe().unwrap());
        c.args([
            "--exact",
            "arrival_is_opt_in_durable_and_preserves_relationship_evidence_through_assessment",
        ])
        .env("L5_GATE_CHILD", "1");
        if enabled {
            c.env("AUGMENTAGENT_ALERT_EMAIL_GATES", "1");
        } else {
            c.env_remove("AUGMENTAGENT_ALERT_EMAIL_GATES");
        }
        let output = c.output().unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
