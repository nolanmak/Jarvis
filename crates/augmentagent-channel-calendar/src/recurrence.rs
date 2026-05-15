//! RRULE -> Cadence collapse — deferred to Phase 2 of #82.
//!
//! Phase 1 logs one Meeting-log line per event instance (per #82 §12), so
//! the daemon never needs to parse RRULEs yet. The module is reserved for
//! Phase 2's "fetch master, collapse 52 weekly bullets into one" pass.

/// Inferred meeting cadence used by the Phase 2 series-collapse path.
///
/// Wired up *only* by tests today so the type doesn't bit-rot before
/// Phase 2 lands. Real construction happens in `cadence_from_rrule()` then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cadence {
    Daily,
    Weekly,
    BiWeekly,
    Monthly,
    /// Anything we don't recognise — fall back to verbatim RRULE in parens.
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that the placeholder enum compiles + round-trips through
    /// the standard derives. Phase 2 lands the actual parser.
    #[test]
    fn cadence_variants_compile() {
        let _ = Cadence::Weekly;
        let _ = Cadence::BiWeekly;
        let _ = Cadence::Monthly;
        let _ = Cadence::Daily;
        let _ = Cadence::Other("RRULE:FREQ=YEARLY".into());
    }
}
