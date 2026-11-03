//! D6 — every place a card can be posted, and how it is covered: through
//! the `AutonomyBroker` (`Wrapped`), or exempt with a reason. The
//! `all_post_approval_sites_wrapped_or_exempt` test scans the workspace and
//! fails on any site this table does not list.

/// How a card-posting site is covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Calls the broker `serve` builds, which `build_approval_surfaces`
    /// wraps in `AutonomyBroker`.
    Wrapped,
    /// Never gated, for the stated reason.
    Exempt(&'static str),
}

/// `(file, marker, count, coverage)`.
pub const EXEMPT_CARD_PATHS: &[(&str, &str, usize, Coverage)] = &[];

/// Count each card-posting marker per file under `root/crates/*/src`,
/// skipping comments, `#[cfg(test)]` modules, `*_tests.rs` and `tests/`.
pub fn scan_card_sites(root: &std::path::Path) -> std::collections::BTreeMap<(String, &'static str), usize> {
    let _ = root;
    todo!("C7: call-site scan")
}
