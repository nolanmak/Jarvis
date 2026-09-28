use augmentagent_channel_core::native_session::{Launch, NativeSession};
use augmentagent_channel_core::providers::ProviderKind;

#[test]
fn claude_create_then_resume_preserves_the_observed_session_id() {
    let session = NativeSession::new(ProviderKind::Claude).unwrap();
    let mut lease = session.begin(ProviderKind::Claude).unwrap();
    let Launch::Create { requested_id: Some(id) } = lease.launch() else { panic!("expected a new Claude session") };
    assert!(uuid::Uuid::parse_str(&id).is_ok());
    lease.observe(&id).unwrap();
    lease.finish().unwrap();
    let mut resumed = session.begin(ProviderKind::Claude).unwrap();
    assert_eq!(resumed.launch(), Launch::Resume { id: id.clone() });
    resumed.observe(&id).unwrap();
    resumed.finish().unwrap();
    assert_eq!(session.id().as_deref(), Some(id.as_str()));
}

#[test]
fn codex_requires_an_observed_thread_id_before_resume() {
    let session = NativeSession::new(ProviderKind::Codex).unwrap();
    let mut lease = session.begin(ProviderKind::Codex).unwrap();
    assert_eq!(lease.launch(), Launch::Create { requested_id: None });
    lease.observe("codex-thread-1").unwrap();
    assert_eq!(session.id().as_deref(), Some("codex-thread-1"));
    lease.finish().unwrap();
    let mut resumed = session.begin(ProviderKind::Codex).unwrap();
    assert_eq!(resumed.launch(), Launch::Resume { id: "codex-thread-1".into() });
    assert!(resumed.observe("different-thread").is_err());
}

#[test]
fn concurrent_or_uncertain_writers_are_rejected() {
    let session = NativeSession::new(ProviderKind::Codex).unwrap();
    let lease = session.begin(ProviderKind::Codex).unwrap();
    assert!(session.begin(ProviderKind::Codex).is_err());
    assert!(session.begin(ProviderKind::Claude).is_err());
    drop(lease);
    assert!(session.begin(ProviderKind::Codex).is_err());
    assert!(session.is_uncertain());
}

#[test]
fn observed_id_is_available_for_uncertain_turn_persistence() {
    let session = NativeSession::new(ProviderKind::Codex).unwrap();
    let mut lease = session.begin(ProviderKind::Codex).unwrap();
    lease.observe("observed-thread").unwrap();
    drop(lease);
    assert_eq!(session.id().as_deref(), Some("observed-thread"));
    assert!(session.is_uncertain());
}

#[test]
fn first_text_output_timestamp_is_set_once_and_reset_for_the_next_turn() {
    let session = NativeSession::new(ProviderKind::Codex).unwrap();
    let mut lease = session.begin(ProviderKind::Codex).unwrap();
    assert_eq!(session.first_text_output_at_ms(), None);
    assert_eq!(session.native_submitted_at_ms(), None);
    lease.observe_native_submission();
    let submitted = session.native_submitted_at_ms().unwrap();
    lease.observe_native_submission();
    assert_eq!(session.native_submitted_at_ms(), Some(submitted));
    lease.observe_first_text_output();
    let first = session.first_text_output_at_ms().unwrap();
    assert!(first >= submitted);
    lease.observe_first_text_output();
    assert_eq!(session.first_text_output_at_ms(), Some(first));
    lease.observe("thread-1").unwrap();
    lease.finish().unwrap();
    let mut next = session.begin(ProviderKind::Codex).unwrap();
    assert_eq!(session.first_text_output_at_ms(), None);
    assert_eq!(session.native_submitted_at_ms(), None);
    next.observe_native_submission();
    next.observe_first_text_output();
    assert!(session.first_text_output_at_ms().unwrap() >= first);
    next.observe("thread-1").unwrap();
    next.finish().unwrap();
}
