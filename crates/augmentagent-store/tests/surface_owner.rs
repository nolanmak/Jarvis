//! #1286 / #1230 — durable owner binding, control conversations and the
//! rejection audit log on the shared surface refs. Temporary stores only;
//! identifiers are synthetic.

use augmentagent_store::owner::{ControlConversationKind, NewAuthRejection, SurfaceOwnerBinding};
use augmentagent_store::{
    Store, StoreError, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};

const T0: i64 = 1_700_000_000_000;

fn account(platform: &str, id: &str) -> SurfaceAccountRef {
    SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), id).unwrap()
}

fn slack_account() -> SurfaceAccountRef {
    account("slack", "team:T00000001")
}

fn owner(account: &SurfaceAccountRef, id: &str) -> SurfaceOwnerRef {
    SurfaceOwnerRef::new(account.clone(), id).unwrap()
}

fn conv(account: &SurfaceAccountRef, id: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(account.clone(), id, None).unwrap()
}

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    (dir, store)
}

#[test]
fn unbound_account_has_no_binding() {
    let (_dir, store) = temp_store();
    assert_eq!(store.surface_owner_binding(&slack_account()).unwrap(), None);
}

#[test]
fn binding_round_trips_across_reopen_with_control_conversations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let acct = slack_account();
    {
        let store = Store::open(&path).unwrap();
        store
            .bind_surface_owner(&owner(&acct, "U00000001"), T0)
            .unwrap();
        store
            .set_surface_control_conversation(
                &conv(&acct, "D00000001"),
                ControlConversationKind::Direct,
                T0 + 1,
            )
            .unwrap();
        store
            .set_surface_control_conversation(
                &conv(&acct, "C00000001"),
                ControlConversationKind::Channel,
                T0 + 2,
            )
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let binding = store.surface_owner_binding(&acct).unwrap().expect("bound");
    assert_eq!(binding.owner, owner(&acct, "U00000001"));
    assert_eq!(binding.confirmed_at_ms, T0);
    assert_eq!(
        binding.direct_conversation(),
        Some(&conv(&acct, "D00000001"))
    );
    assert_eq!(binding.control_channel(), Some(&conv(&acct, "C00000001")));
    assert!(binding.is_control_conversation("D00000001"));
    assert!(binding.is_control_conversation("C00000001"));
    assert!(!binding.is_control_conversation("C00000002"));
}

#[test]
fn bindings_are_keyed_by_platform_and_account() {
    let (_dir, store) = temp_store();
    let t1 = slack_account();
    let t2 = account("slack", "team:T00000002");
    let grid = account("slack", "enterprise:E00000001/team:T00000001");
    let wa = account("whatsapp", "team:T00000001");
    store
        .bind_surface_owner(&owner(&t1, "U00000001"), T0)
        .unwrap();

    assert!(store.surface_owner_binding(&t1).unwrap().is_some());
    for other in [&t2, &grid, &wa] {
        assert_eq!(
            store.surface_owner_binding(other).unwrap(),
            None,
            "{other:?} must not inherit the T00000001 binding"
        );
    }
}

#[test]
fn only_one_control_conversation_per_kind_and_replacing_it_moves_it() {
    let (_dir, store) = temp_store();
    let acct = slack_account();
    store
        .bind_surface_owner(&owner(&acct, "U00000001"), T0)
        .unwrap();
    store
        .set_surface_control_conversation(
            &conv(&acct, "C00000001"),
            ControlConversationKind::Channel,
            T0,
        )
        .unwrap();
    store
        .set_surface_control_conversation(
            &conv(&acct, "C00000002"),
            ControlConversationKind::Channel,
            T0 + 1,
        )
        .unwrap();
    let binding = store.surface_owner_binding(&acct).unwrap().unwrap();
    assert_eq!(binding.control.len(), 1);
    assert_eq!(binding.control_channel(), Some(&conv(&acct, "C00000002")));
    assert!(!binding.is_control_conversation("C00000001"));
}

#[test]
fn same_conversation_cannot_be_both_direct_and_channel() {
    let (_dir, store) = temp_store();
    let acct = slack_account();
    store
        .bind_surface_owner(&owner(&acct, "U00000001"), T0)
        .unwrap();
    let c = conv(&acct, "C00000001");
    store
        .set_surface_control_conversation(&c, ControlConversationKind::Channel, T0)
        .unwrap();
    store
        .set_surface_control_conversation(&c, ControlConversationKind::Direct, T0 + 1)
        .unwrap();
    let binding = store.surface_owner_binding(&acct).unwrap().unwrap();
    assert_eq!(binding.control.len(), 1);
    assert_eq!(binding.direct_conversation(), Some(&c));
    assert_eq!(binding.control_channel(), None);
}

#[test]
fn control_conversation_requires_a_binding_and_no_thread() {
    let (_dir, store) = temp_store();
    let acct = slack_account();
    let err = store
        .set_surface_control_conversation(
            &conv(&acct, "C00000001"),
            ControlConversationKind::Channel,
            T0,
        )
        .unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidInput(ref m) if m.contains("not bound")),
        "{err:?}"
    );

    store
        .bind_surface_owner(&owner(&acct, "U00000001"), T0)
        .unwrap();
    let threaded =
        SurfaceConversationRef::new(acct.clone(), "C00000001", Some("1700000000.000100".into()))
            .unwrap();
    let err = store
        .set_surface_control_conversation(&threaded, ControlConversationKind::Channel, T0)
        .unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidInput(ref m) if m.contains("thread")),
        "{err:?}"
    );
}

#[test]
fn rebinding_the_same_owner_keeps_control_conversations() {
    let (_dir, store) = temp_store();
    let acct = slack_account();
    store
        .bind_surface_owner(&owner(&acct, "U00000001"), T0)
        .unwrap();
    store
        .set_surface_control_conversation(
            &conv(&acct, "C00000001"),
            ControlConversationKind::Channel,
            T0,
        )
        .unwrap();
    store
        .bind_surface_owner(&owner(&acct, "U00000001"), T0 + 5)
        .unwrap();
    let binding = store.surface_owner_binding(&acct).unwrap().unwrap();
    assert_eq!(binding.confirmed_at_ms, T0 + 5);
    assert!(binding.is_control_conversation("C00000001"));
}

#[test]
fn binding_a_different_owner_drops_the_previous_control_conversations() {
    let (_dir, store) = temp_store();
    let acct = slack_account();
    store
        .bind_surface_owner(&owner(&acct, "U00000001"), T0)
        .unwrap();
    store
        .set_surface_control_conversation(
            &conv(&acct, "D00000001"),
            ControlConversationKind::Direct,
            T0,
        )
        .unwrap();
    store
        .bind_surface_owner(&owner(&acct, "U00000002"), T0 + 5)
        .unwrap();
    let binding = store.surface_owner_binding(&acct).unwrap().unwrap();
    assert_eq!(binding.owner.sender_id(), "U00000002");
    assert!(
        binding.control.is_empty(),
        "a new owner must not inherit the old owner's DM: {binding:?}"
    );
}

#[test]
fn unbind_removes_binding_and_control_conversations_only_for_that_account() {
    let (_dir, store) = temp_store();
    let t1 = slack_account();
    let t2 = account("slack", "team:T00000002");
    for acct in [&t1, &t2] {
        store
            .bind_surface_owner(&owner(acct, "U00000001"), T0)
            .unwrap();
        store
            .set_surface_control_conversation(
                &conv(acct, "C00000001"),
                ControlConversationKind::Channel,
                T0,
            )
            .unwrap();
    }
    assert!(store.unbind_surface_owner(&t1).unwrap());
    assert!(
        !store.unbind_surface_owner(&t1).unwrap(),
        "second unbind is a no-op"
    );
    assert_eq!(store.surface_owner_binding(&t1).unwrap(), None);
    // Re-binding must start clean, not resurrect the old control channel.
    store
        .bind_surface_owner(&owner(&t1, "U00000001"), T0 + 1)
        .unwrap();
    assert!(store
        .surface_owner_binding(&t1)
        .unwrap()
        .unwrap()
        .control
        .is_empty());
    assert!(store
        .surface_owner_binding(&t2)
        .unwrap()
        .unwrap()
        .is_control_conversation("C00000001"));
}

#[test]
fn remove_control_conversation_reports_whether_it_existed() {
    let (_dir, store) = temp_store();
    let acct = slack_account();
    store
        .bind_surface_owner(&owner(&acct, "U00000001"), T0)
        .unwrap();
    let c = conv(&acct, "C00000001");
    store
        .set_surface_control_conversation(&c, ControlConversationKind::Channel, T0)
        .unwrap();
    assert!(store.remove_surface_control_conversation(&c).unwrap());
    assert!(!store.remove_surface_control_conversation(&c).unwrap());
    assert!(store
        .surface_owner_binding(&acct)
        .unwrap()
        .unwrap()
        .control
        .is_empty());
}

#[test]
fn list_bindings_for_platform() {
    let (_dir, store) = temp_store();
    let t1 = slack_account();
    let t2 = account("slack", "team:T00000002");
    let wa = account("whatsapp", "device:1");
    store
        .bind_surface_owner(&owner(&t2, "U00000002"), T0)
        .unwrap();
    store
        .bind_surface_owner(&owner(&t1, "U00000001"), T0)
        .unwrap();
    store.bind_surface_owner(&owner(&wa, "owner"), T0).unwrap();
    let slack = SurfacePlatform::new("slack").unwrap();
    let all: Vec<SurfaceOwnerBinding> = store.surface_owner_bindings(&slack).unwrap();
    let ids: Vec<&str> = all.iter().map(|b| b.owner.account().account_id()).collect();
    assert_eq!(ids, vec!["team:T00000001", "team:T00000002"]);
}

#[test]
fn rejection_audit_is_appended_and_listed_newest_first_per_account() {
    let (_dir, store) = temp_store();
    let acct = slack_account();
    let other = account("slack", "team:T00000002");
    let first = store
        .record_surface_auth_rejection(&NewAuthRejection {
            account: acct.clone(),
            conversation_id: Some("C00000001".into()),
            actor_id: Some("U00000002".into()),
            event_kind: "message".into(),
            event_id: Some("Ev00000001".into()),
            reason: "not_owner".into(),
            occurred_at_ms: T0,
        })
        .unwrap();
    let second = store
        .record_surface_auth_rejection(&NewAuthRejection {
            account: acct.clone(),
            conversation_id: None,
            actor_id: None,
            event_kind: "interaction".into(),
            event_id: None,
            reason: "missing_actor".into(),
            occurred_at_ms: T0 + 1,
        })
        .unwrap();
    store
        .record_surface_auth_rejection(&NewAuthRejection {
            account: other.clone(),
            conversation_id: None,
            actor_id: Some("U00000001".into()),
            event_kind: "message".into(),
            event_id: None,
            reason: "unbound_workspace".into(),
            occurred_at_ms: T0 + 2,
        })
        .unwrap();
    assert!(second > first);

    let rows = store.surface_auth_rejections(&acct, 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, second);
    assert_eq!(rows[0].reason, "missing_actor");
    assert_eq!(rows[1].reason, "not_owner");
    assert_eq!(rows[1].actor_id.as_deref(), Some("U00000002"));
    assert_eq!(rows[1].conversation_id.as_deref(), Some("C00000001"));
    assert_eq!(rows[1].event_id.as_deref(), Some("Ev00000001"));
    assert_eq!(store.surface_auth_rejections(&acct, 1).unwrap().len(), 1);
    assert_eq!(store.surface_auth_rejection_count(&acct).unwrap(), 2);
}

#[test]
fn rejection_audit_refuses_unbounded_fields() {
    let (_dir, store) = temp_store();
    let err = store
        .record_surface_auth_rejection(&NewAuthRejection {
            account: slack_account(),
            conversation_id: None,
            actor_id: None,
            event_kind: "message".into(),
            event_id: None,
            reason: "x".repeat(10_000),
            occurred_at_ms: T0,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
}

#[test]
fn migration_is_idempotent_on_an_existing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let acct = slack_account();
    Store::open(&path)
        .unwrap()
        .bind_surface_owner(&owner(&acct, "U00000001"), T0)
        .unwrap();
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        assert!(store.surface_owner_binding(&acct).unwrap().is_some());
    }
}
