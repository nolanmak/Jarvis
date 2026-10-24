//! #1551 additive per-turn latency table and nearest-rank percentiles.
use augmentagent_store::{
    phone::{nearest_rank_percentile, PhoneTurnMetrics, TurnOutcome, TurnProvider},
    Store,
};

fn record(call: &str, turn: u64, recorded_ms: i64, first_speech: i64) -> PhoneTurnMetrics {
    PhoneTurnMetrics {
        call_id: call.into(),
        turn_id: format!("phone:{call}:{turn}"),
        recorded_ms,
        provider: TurnProvider::Native,
        outcome: TurnOutcome::Answered,
        queue_wait_ms: Some(10),
        t_first_token_ms: Some(first_speech - 20),
        t_first_speech_ms: Some(first_speech),
        t_last_speech_ms: Some(first_speech + 1_000),
        tool_ms: None,
        t_first_played_ms: None,
        t_last_played_ms: None,
        audible_delta_ms: None,
        commit_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
    }
}

#[test]
fn nearest_rank_percentiles_match_hand_computed_fixture() {
    assert_eq!(nearest_rank_percentile(&[], 50), None);
    assert_eq!(nearest_rank_percentile(&[300], 50), Some(300));
    assert_eq!(nearest_rank_percentile(&[300], 95), Some(300));
    // ceil(0.5*3)=2nd and ceil(0.95*3)=3rd values.
    assert_eq!(nearest_rank_percentile(&[300, 320, 1_200], 50), Some(320));
    assert_eq!(nearest_rank_percentile(&[300, 320, 1_200], 95), Some(1_200));
    // 20 samples: p50 is the 10th, p95 the 19th.
    let twenty: Vec<i64> = (1..=20).map(|n| n * 100).collect();
    assert_eq!(nearest_rank_percentile(&twenty, 50), Some(1_000));
    assert_eq!(nearest_rank_percentile(&twenty, 95), Some(1_900));
}

#[test]
fn ac03_three_records_yield_fixture_p50_and_p95_for_first_speech() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("metrics.db")).unwrap();
    assert!(store.phone_record_turn_metrics(&record("call-a", 1, 1_000, 300)).unwrap());
    assert!(store.phone_record_turn_metrics(&record("call-a", 2, 2_000, 1_200)).unwrap());
    assert!(store.phone_record_turn_metrics(&record("call-b", 1, 3_000, 320)).unwrap());
    // Exactly one row per turn: a replay is ignored, not duplicated.
    assert!(!store.phone_record_turn_metrics(&record("call-a", 2, 2_500, 999)).unwrap());
    assert_eq!(store.phone_turn_metrics("call-a").unwrap().len(), 2);
    assert_eq!(
        store.phone_turn_metrics("call-a").unwrap()[1].t_first_speech_ms,
        Some(1_200)
    );

    let summary = store.phone_turn_metrics_summary(10).unwrap();
    assert_eq!(summary.calls, 2);
    assert_eq!(summary.turns, 3);
    let first_speech = summary
        .fields
        .iter()
        .find(|f| f.field == "t_first_speech_ms")
        .unwrap();
    assert_eq!(first_speech.samples, 3);
    assert_eq!(first_speech.p50, Some(320));
    assert_eq!(first_speech.p95, Some(1_200));
    let tool = summary.fields.iter().find(|f| f.field == "tool_ms").unwrap();
    assert_eq!((tool.samples, tool.p50, tool.p95), (0, None, None));
    assert!(summary.fields.iter().any(|f| f.field == "audible_delta_ms"));

    // `--last 1` restricts to the most recently recorded call only.
    let latest = store.phone_turn_metrics_summary(1).unwrap();
    assert_eq!((latest.calls, latest.turns), (1, 1));
    let first_speech = latest
        .fields
        .iter()
        .find(|f| f.field == "t_first_speech_ms")
        .unwrap();
    assert_eq!((first_speech.p50, first_speech.p95), (Some(320), Some(320)));
    let empty = store.phone_turn_metrics_summary(0).unwrap();
    assert_eq!((empty.calls, empty.turns), (0, 0));
}

#[test]
fn ac04_records_reject_identifiers_that_could_carry_private_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("metrics.db")).unwrap();
    let mut bad = record("call-a", 1, 1_000, 300);
    bad.commit_sha = Some("not hex +12025550123".into());
    assert!(store.phone_record_turn_metrics(&bad).is_err());
    let mut bad = record("call-a", 1, 1_000, 300);
    bad.turn_id = "phone:call-a:1\nvoicePrompt".into();
    assert!(store.phone_record_turn_metrics(&bad).is_err());
    let mut bad = record("call-a", 1, 1_000, 300);
    bad.t_first_speech_ms = Some(-5);
    assert!(store.phone_record_turn_metrics(&bad).is_err());
    assert!(store.phone_turn_metrics("call-a").unwrap().is_empty());
}
