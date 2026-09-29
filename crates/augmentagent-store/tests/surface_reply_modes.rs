//! #1297 — the per-conversation reply mode behind Slack's `voice on|off`:
//! stored by conversation, exact (no inheritance here), cleared by `None`,
//! and still there after the database is reopened. Temporary stores and
//! synthetic identifiers only.

use augmentagent_store::{Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform};

const T0: i64 = 1_700_000_000_000;

fn conversation(channel: &str, thread: Option<&str>) -> SurfaceConversationRef {
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "T00000001").unwrap();
    SurfaceConversationRef::new(account, channel, thread.map(str::to_string)).unwrap()
}

#[test]
fn reply_mode_is_per_conversation_and_survives_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let dm = conversation("D00000001", None);
    let thread = conversation("D00000001", Some("1700000000.000100"));
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.surface_reply_mode(&dm).unwrap(), None);
        store
            .set_surface_reply_mode(&dm, Some("spoken"), T0)
            .unwrap();
        assert_eq!(
            store.surface_reply_mode(&dm).unwrap().as_deref(),
            Some("spoken")
        );
        // Exact lookup: a thread of the DM has no row of its own.
        assert_eq!(store.surface_reply_mode(&thread).unwrap(), None);
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.surface_reply_mode(&dm).unwrap().as_deref(),
        Some("spoken")
    );
    store
        .set_surface_reply_mode(&dm, Some("text"), T0 + 1)
        .unwrap();
    assert_eq!(
        store.surface_reply_mode(&dm).unwrap().as_deref(),
        Some("text")
    );
    store.set_surface_reply_mode(&dm, None, T0 + 2).unwrap();
    assert_eq!(store.surface_reply_mode(&dm).unwrap(), None);
}

#[test]
fn an_empty_reply_mode_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let dm = conversation("D00000001", None);
    assert!(store.set_surface_reply_mode(&dm, Some("  "), T0).is_err());
    assert_eq!(store.surface_reply_mode(&dm).unwrap(), None);
}
