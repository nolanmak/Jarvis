//! `augmentagent phone metrics --last N` (#1551): nearest-rank p50/p95 per
//! latency field over the last N calls. Reads `phone_turn_metrics` only; it
//! prints no speech text, numbers or provider identifiers.
use anyhow::Result;
use augmentagent_store::{phone::PhoneMetricsSummary, Store};

fn cell(value: Option<i64>) -> String {
    value.map_or_else(|| "-".into(), |v| v.to_string())
}
pub fn render(summary: &PhoneMetricsSummary) -> String {
    let mut out = format!(
        "phone turn metrics: last {} calls, {} turns (nearest-rank, ms)\n",
        summary.calls, summary.turns
    );
    out.push_str(&format!("{:<20} {:>7} {:>6} {:>6}\n", "field", "samples", "p50", "p95"));
    for field in &summary.fields {
        out.push_str(&format!(
            "{:<20} {:>7} {:>6} {:>6}\n",
            field.field,
            field.samples,
            cell(field.p50),
            cell(field.p95)
        ));
    }
    out
}
pub fn run(store: &Store, last: usize) -> Result<()> {
    let summary = store.phone_turn_metrics_summary(last)?;
    print!("{}", render(&summary));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::phone::{PhoneTurnMetrics, TurnOutcome, TurnProvider};
    use augmentagent_store::Store;

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
            t_first_played_ms: Some(first_speech + 290),
            t_last_played_ms: Some(first_speech + 1_500),
            audible_delta_ms: Some(290),
            commit_sha: None,
        }
    }

    #[test]
    fn ac03_table_has_header_and_hand_computed_percentile_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("metrics.db")).unwrap();
        store.phone_record_turn_metrics(&record("call-a", 1, 1_000, 300)).unwrap();
        store.phone_record_turn_metrics(&record("call-a", 2, 2_000, 1_200)).unwrap();
        store.phone_record_turn_metrics(&record("call-b", 1, 3_000, 320)).unwrap();
        let table = render(&store.phone_turn_metrics_summary(5).unwrap());
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[0], "phone turn metrics: last 2 calls, 3 turns (nearest-rank, ms)");
        assert_eq!(lines[1], "field                samples    p50    p95");
        let row = lines
            .iter()
            .find(|line| line.starts_with("t_first_speech_ms"))
            .expect("row");
        assert_eq!(*row, "t_first_speech_ms          3    320   1200");
        let delta = lines.iter().find(|l| l.starts_with("audible_delta_ms")).unwrap();
        assert_eq!(*delta, "audible_delta_ms           3    290    290");
        let tool = lines.iter().find(|l| l.starts_with("tool_ms")).unwrap();
        assert_eq!(*tool, "tool_ms                    0      -      -");
        assert_eq!(lines.len(), 2 + 8);
    }

    #[test]
    fn empty_database_renders_an_empty_table_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("metrics.db")).unwrap();
        let table = render(&store.phone_turn_metrics_summary(10).unwrap());
        assert!(table.starts_with("phone turn metrics: last 0 calls, 0 turns"));
        assert!(table.contains("t_first_played_ms          0      -      -"));
        assert!(run(&store, 10).is_ok());
    }
}
