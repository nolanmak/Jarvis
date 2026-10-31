//! Transport evidence only. These counters cannot establish audible response
//! latency; that requires an independent caller-side timing measurement.
use augmentagent_store::phone::PhoneLivePlayback;
use tokio::time::Instant;
pub struct PlaybackTracker {
    start: Instant,
    evidence: PhoneLivePlayback,
}
impl Default for PlaybackTracker {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            evidence: PhoneLivePlayback::default(),
        }
    }
}
impl PlaybackTracker {
    fn elapsed(&self) -> u64 {
        self.start
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
    pub fn snapshot(&self) -> PhoneLivePlayback {
        self.evidence.clone()
    }
    pub fn input(&mut self, bytes: usize) {
        self.evidence.input_bytes = self.evidence.input_bytes.saturating_add(bytes as u64);
    }
    pub fn received_output(&mut self, bytes: usize) {
        let elapsed = self.elapsed();
        self.evidence
            .first_output_received_ms
            .get_or_insert(elapsed);
        self.evidence.output_received_bytes = self
            .evidence
            .output_received_bytes
            .saturating_add(bytes as u64);
    }
    pub fn sent_output(&mut self, bytes: usize, pending: usize) {
        let elapsed = self.elapsed();
        self.evidence.first_output_sent_ms.get_or_insert(elapsed);
        self.evidence.output_sent_bytes =
            self.evidence.output_sent_bytes.saturating_add(bytes as u64);
        self.pending(pending);
    }
    pub fn played(&mut self, bytes: usize, pending: usize) {
        let elapsed = self.elapsed();
        self.evidence
            .first_playback_receipt_ms
            .get_or_insert(elapsed);
        self.evidence.played_bytes = self.evidence.played_bytes.saturating_add(bytes as u64);
        self.pending(pending);
    }
    pub fn clear_requested(&mut self, bytes: usize) {
        self.evidence.clear_requested_bytes = self
            .evidence
            .clear_requested_bytes
            .saturating_add(bytes as u64);
    }
    pub fn cleared(&mut self, bytes: usize, pending: usize) {
        self.evidence.cleared_receipt_bytes = self
            .evidence
            .cleared_receipt_bytes
            .saturating_add(bytes as u64);
        self.pending(pending);
    }
    pub fn pending(&mut self, bytes: usize) {
        // The transport reserves marks before writing. A failed write can leave
        // reserved bytes that were never sent and cannot await playback.
        let unconfirmed = self.evidence.output_sent_bytes
            .saturating_sub(self.evidence.played_bytes)
            .saturating_sub(self.evidence.cleared_receipt_bytes);
        self.evidence.pending_bytes = (bytes as u64).min(unconfirmed);
        self.evidence.peak_pending_bytes = self.evidence.peak_pending_bytes.max(self.evidence.pending_bytes);
    }
}
fn timing(label: &str, mut values: Vec<u64>) -> String {
    values.sort_unstable();
    let percentile = |p: usize| {
        values
            .get((values.len() * p).div_ceil(100).saturating_sub(1))
            .map_or_else(|| "unavailable".into(), |v| v.to_string())
    };
    format!(
        "{label}: samples={} p50={} p95={}\n",
        values.len(),
        percentile(50),
        percentile(95)
    )
}
pub fn render_playback(rows: &[(PhoneLivePlayback, bool)]) -> String {
    let sum = |get: fn(&PhoneLivePlayback) -> u64| {
        rows.iter().map(|(e, _)| u128::from(get(e))).sum::<u128>()
    };
    let sent = sum(|e| e.output_sent_bytes);
    let played = sum(|e| e.played_bytes);
    let cleared = sum(|e| e.cleared_receipt_bytes);
    let mut out=format!("Live playback evidence: calls={} collectors_closed={}\ninput_bytes={} output_received_bytes={} output_sent_bytes={sent} played_bytes={played} clear_requested_bytes={} cleared_receipt_bytes={cleared} unconfirmed_sent_bytes={}\n",rows.len(),rows.iter().filter(|(_,closed)|*closed).count(),sum(|e|e.input_bytes),sum(|e|e.output_received_bytes),sum(|e|e.clear_requested_bytes),sent.saturating_sub(played+cleared));
    out.push_str(&timing(
        "server_first_output_ms_since_bridge_start",
        rows.iter()
            .filter_map(|(e, _)| e.first_output_received_ms)
            .collect(),
    ));
    out.push_str(&timing(
        "first_playback_receipt_ms_since_bridge_start",
        rows.iter()
            .filter_map(|(e, _)| e.first_playback_receipt_ms)
            .collect(),
    ));
    out.push_str(
        "audible_response_latency_ms=unavailable (marks report buffer drain, not speech onset)\n",
    );
    out
}
