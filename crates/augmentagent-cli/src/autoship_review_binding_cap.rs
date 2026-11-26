//! #2451: bound review attempts and review model calls per reviewed binding.
//!
//! #2421 bounds one review attempt (~74 calls). Nothing bounded how many
//! attempts ran on the same code: PR #2378 reviewed head `b260fb89` three
//! times and `6b52b713` twice under one contract, each time reaching the same
//! rejection, while holding the worker's only slot. A reviewed binding is the
//! candidate head plus its registered contract. The verification evidence hash
//! folds in per-run receipt ids, so re-verifying an unchanged head always
//! yields a new hash; keying on it would never see a repeat.
use super::*;

/// Review attempts admitted for one (head, contract) before a rejection must
/// be acted on (repair or rescope) instead of re-reviewed.
pub(crate) const MAX_REVIEW_ATTEMPTS_PER_BINDING: u32 = 2;
/// Adjudicated amendment rounds (dispute → amend → re-review) per acceptance
/// source and original base before the remaining findings go to rescoping.
/// #2572: attempts that ended without findings (aborted, budget, capacity)
/// leave nothing to repair. They may continue up to this many per binding while
/// the binding's call budget remains, instead of backing off forever.
pub(crate) const MAX_UNPRODUCTIVE_REVIEW_ATTEMPTS: u32 = 4;
pub(crate) const MAX_ADJUDICATION_ROUNDS: u32 = 2;
pub(crate) const ADJUDICATION_ROUND_PURPOSE: &str = "adjudicated-amendment-round";
const ATTEMPT_PURPOSE: &str = "review-binding-attempt";
const CALLS_PURPOSE: &str = "review-binding-calls";
/// Bounded wait before an exhausted binding with no actionable findings is
/// looked at again. Re-entry spends no model call: the cap is checked first.
pub(crate) const EXHAUSTED_BACKOFF_MS: i64 = 3_600_000;

pub(crate) fn binding_key(head: &str, contract: &str) -> String {
    format!("{:x}", Sha256::digest(format!("review-binding-v1\0{head}\0{contract}").as_bytes()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Usage {
    pub attempts: u32,
    pub calls: u64,
}

/// What the caller does with a binding about to be reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// Spend a review attempt.
    Review,
    /// Do not re-review: repair against the findings already recorded.
    Repair,
    /// Findings remain but no repair round is left: rescope them.
    Rescope,
    /// Nothing actionable is recorded: wait, bounded, without spending.
    Backoff,
}

impl Route {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Route::Review => "review_binding_admitted",
            Route::Repair => "review_attempts_exhausted_repair",
            Route::Rescope => "review_attempts_exhausted_rescope",
            Route::Backoff => "review_attempts_exhausted_backoff",
        }
    }
}

/// The per-binding call budget: every admitted attempt at #2421's worst case.
pub(crate) fn call_cap(per_attempt_bound: u64) -> u64 {
    per_attempt_bound.saturating_mul(u64::from(MAX_REVIEW_ATTEMPTS_PER_BINDING))
}

/// Pure routing. A new attempt is admitted only while the attempt count is
/// under the cap and its worst case still fits the binding's call budget, so
/// one over-spent attempt (fallback reviewer, adjudication) cannot buy another
/// re-review of findings already recorded. With no findings recorded, the
/// unproductive-attempt bound and the call budget decide (#2572).
pub(crate) fn route(usage: Usage, per_attempt_bound: u64, has_findings: bool, repair_round_left: bool) -> Route {
    let fits = usage.calls.saturating_add(per_attempt_bound) <= call_cap(per_attempt_bound);
    if usage.attempts < MAX_REVIEW_ATTEMPTS_PER_BINDING && fits {
        Route::Review
    } else if !has_findings {
        // #2572: nothing recorded to repair or rescope. An attempt that never
        // reached a verdict is not a rejection, so it may not strand unchanged
        // code; another attempt runs while the binding's budget remains.
        if usage.attempts < MAX_UNPRODUCTIVE_REVIEW_ATTEMPTS && usage.calls < call_cap(per_attempt_bound) {
            Route::Review
        } else {
            Route::Backoff
        }
    } else if repair_round_left {
        Route::Repair
    } else {
        Route::Rescope
    }
}

fn counter(tx: &Transaction<'_>, workflow: &str, purpose: &str, key: &str) -> Result<u64> {
    let value: Option<i64> = tx.query_row(
        "SELECT attempts FROM bounded_attempt WHERE workflow=?1 AND purpose=?2 AND binding=?3",
        params![workflow, purpose, format!("review:{key}")], |row| row.get(0)).optional()?;
    Ok(u64::try_from(value.unwrap_or(0))?)
}

fn add(tx: &Transaction<'_>, workflow: &str, purpose: &str, key: &str, amount: u64) -> Result<()> {
    let amount = i64::try_from(amount)?;
    tx.execute("INSERT INTO bounded_attempt(workflow,purpose,binding,attempts) VALUES(?1,?2,?3,?4)
        ON CONFLICT(workflow,purpose,binding) DO UPDATE SET attempts=attempts+excluded.attempts",
        params![workflow, purpose, format!("review:{key}"), amount])?;
    Ok(())
}

fn usage_in(tx: &Transaction<'_>, workflow: &str, key: &str) -> Result<Usage> {
    Ok(Usage { attempts: u32::try_from(counter(tx, workflow, ATTEMPT_PURPOSE, key)?)?,
        calls: counter(tx, workflow, CALLS_PURPOSE, key)? })
}

fn valid_key(key: &str) -> Result<()> {
    ensure!(key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()), "review binding key must be a digest");
    Ok(())
}

/// Recorded findings a capped binding is repaired against: only a structured
/// review bound to this exact head and contract, without findings the final
/// adjudication dismissed. Upheld rulings contribute their repair criterion.
pub(crate) fn recorded_findings(review: &crate::autoship_review::ReviewEvidence, head: &str, contract: &str) -> Option<String> {
    use crate::autoship_review::{AdjudicationReceipt, Ruling};
    if review.binding.head != head || review.binding.contract_hash != contract {
        return None;
    }
    fn rulings<'a>(receipt: &'a AdjudicationReceipt, out: &mut std::collections::BTreeMap<&'a str, &'a Ruling>) {
        for (id, ruling) in &receipt.rulings { out.insert(id.as_str(), ruling); }
        for component in &receipt.components { rulings(component, out); }
    }
    let mut ruled = std::collections::BTreeMap::new();
    if let Some(receipt) = &review.adjudication { rulings(receipt, &mut ruled); }
    let mut seen = std::collections::BTreeSet::new();
    let mut lines = vec![];
    for finding in review.passes.iter().flat_map(|pass| &pass.findings) {
        if !seen.insert(finding.id.as_str()) { continue; }
        match ruled.get(finding.id.as_str()) {
            Some(Ruling::Dismiss { .. }) => {}
            Some(Ruling::Uphold { repair_criterion, .. }) => lines.push(format!(
                "- [{}] {}\n  Required repair: {}", finding.criterion_id, finding.claim.trim(), repair_criterion.trim())),
            _ => lines.push(format!("- [{}] {}\n  Requested change: {}",
                finding.criterion_id, finding.claim.trim(), finding.requested_change.trim())),
        }
    }
    (!lines.is_empty()).then(|| format!(
        "Review findings already recorded for this exact head and contract (re-review is capped at {MAX_REVIEW_ATTEMPTS_PER_BINDING} attempts):\n{}",
        lines.join("\n")))
}

/// Durable adjudication-round key: the acceptance source and original base.
/// Neither an amended contract nor a repaired head resets it.
pub(crate) fn adjudication_round_key(source: &str, baseline_head: &str) -> String {
    format!("{:x}", Sha256::digest(format!("adjudication-round-v1\0{source}\0{baseline_head}").as_bytes()))
}

impl WorkflowStore {
    pub(crate) fn review_binding_usage(&self, lease: &Lease, key: &str, now: i64) -> Result<Usage> {
        valid_key(key)?;
        let tx = self.conn.unchecked_transaction()?;
        require_lease(&tx, lease, now)?;
        let usage = usage_in(&tx, &lease.workflow, key)?;
        tx.commit()?;
        Ok(usage)
    }

    /// Atomically decide and, for `Route::Review`, consume one attempt. Every
    /// refusal is an audited `review_binding_capped` event with its route.
    pub(crate) fn admit_review_binding(&mut self, lease: &Lease, key: &str, per_attempt_bound: u64,
        has_findings: bool, repair_round_left: bool, now: i64) -> Result<Route> {
        valid_key(key)?;
        ensure!(per_attempt_bound > 0, "review attempt bound must be positive");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let usage = usage_in(&tx, &lease.workflow, key)?;
        let route = route(usage, per_attempt_bound, has_findings, repair_round_left);
        let kind = if route == Route::Review {
            add(&tx, &lease.workflow, ATTEMPT_PURPOSE, key, 1)?;
            "review_binding_admitted"
        } else {
            "review_binding_capped"
        };
        event(&tx, &lease.workflow, lease.generation, kind, &serde_json::json!({
            "version": 1, "key": key, "reason": route.reason(),
            "attempts": usage.attempts + u32::from(route == Route::Review), "max_attempts": MAX_REVIEW_ATTEMPTS_PER_BINDING,
            "review_calls": usage.calls, "call_cap": call_cap(per_attempt_bound),
            "has_findings": has_findings, "repair_round_left": repair_round_left,
        }).to_string(), now)?;
        tx.commit()?;
        Ok(route)
    }

    /// Charge the review model calls an attempt actually spent.
    pub(crate) fn charge_review_binding_calls(&mut self, lease: &Lease, key: &str, calls: u64, now: i64) -> Result<()> {
        valid_key(key)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        add(&tx, &lease.workflow, CALLS_PURPOSE, key, calls)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUND: u64 = 74;

    fn fixture() -> (tempfile::TempDir, WorkflowStore, Lease) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let binding = Binding { head: "h".into(), contract: "c".into(), evidence_hash: "e".into() };
        store.register("owner/repo#2329", "agent-fix/issue-2329", binding, 1).unwrap();
        let lease = store.claim("owner/repo#2329", "worker", 2, 1_000_000).unwrap().unwrap();
        (dir, store, lease)
    }

    /// The #2378 shape: the same head and contract is reviewed, rejected with
    /// concrete findings, and selected again. Exactly N attempts run; the next
    /// selection repairs against the recorded findings instead of reviewing.
    #[test]
    fn repeated_rejection_of_one_binding_admits_exactly_n_attempts_then_repairs() {
        let (_dir, mut store, lease) = fixture();
        let key = binding_key("6b52b71342b0e4df9d29b055d1db1392f7def88d", "e32b4a6a");
        let mut reviews = 0;
        let mut routes = vec![];
        for tick in 0..5 {
            let route = store.admit_review_binding(&lease, &key, BOUND, true, true, 10 + tick).unwrap();
            routes.push(route);
            if route == Route::Review {
                reviews += 1;
                store.charge_review_binding_calls(&lease, &key, 40, 10 + tick).unwrap();
            }
        }
        assert_eq!(reviews, MAX_REVIEW_ATTEMPTS_PER_BINDING, "routes: {routes:?}");
        assert_eq!(routes[2..], [Route::Repair, Route::Repair, Route::Repair]);
        assert_eq!(store.review_binding_usage(&lease, &key, 20).unwrap(),
            Usage { attempts: MAX_REVIEW_ATTEMPTS_PER_BINDING, calls: 80 });
        let capped = store.events(&lease.workflow).unwrap().into_iter()
            .filter(|(_, kind, _, _)| kind == "review_binding_capped").count();
        assert_eq!(capped, 3, "every refused re-review is recorded with its reason");
    }

    /// A repair commit is new code: its head gets a fresh attempt budget while
    /// the old binding stays exhausted.
    #[test]
    fn a_changed_head_resets_the_count() {
        let (_dir, mut store, lease) = fixture();
        let old = binding_key("6b52b713", "e32b4a6a");
        for tick in 0..MAX_REVIEW_ATTEMPTS_PER_BINDING {
            assert_eq!(store.admit_review_binding(&lease, &old, BOUND, true, true, 10 + i64::from(tick)).unwrap(), Route::Review);
        }
        assert_eq!(store.admit_review_binding(&lease, &old, BOUND, true, true, 20).unwrap(), Route::Repair);
        let repaired = binding_key("ffadea98", "e32b4a6a");
        assert_ne!(old, repaired);
        assert_eq!(store.admit_review_binding(&lease, &repaired, BOUND, true, true, 21).unwrap(), Route::Review);
        assert_eq!(store.review_binding_usage(&lease, &repaired, 22).unwrap().attempts, 1);
        assert_eq!(store.admit_review_binding(&lease, &old, BOUND, true, true, 23).unwrap(), Route::Repair,
            "returning to an exhausted binding does not reopen it");
        // An amended contract is a new binding too.
        assert_eq!(store.admit_review_binding(&lease, &binding_key("6b52b713", "025dcb47"), BOUND, true, true, 24).unwrap(), Route::Review);
    }

    /// The budget is durable: a restarted worker or a later tick (new lease
    /// generation) cannot reset it.
    #[test]
    fn the_attempt_budget_survives_restart_and_new_leases() {
        let (dir, mut store, lease) = fixture();
        let key = binding_key("b260fb89", "e32b4a6a");
        for tick in 0..MAX_REVIEW_ATTEMPTS_PER_BINDING {
            assert_eq!(store.admit_review_binding(&lease, &key, BOUND, true, true, 10 + i64::from(tick)).unwrap(), Route::Review);
        }
        store.release(&lease, 30).unwrap();
        drop(store);
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let next = store.claim("owner/repo#2329", "worker", 40, 1_000_000).unwrap().unwrap();
        assert_eq!(store.admit_review_binding(&next, &key, BOUND, true, true, 41).unwrap(), Route::Repair);
    }

    /// Routing once the attempt budget is spent, and the call budget built on
    /// #2421's per-attempt worst case.
    #[test]
    fn exhausted_bindings_route_to_repair_rescope_or_bounded_backoff() {
        let spent = Usage { attempts: MAX_REVIEW_ATTEMPTS_PER_BINDING, calls: 100 };
        assert_eq!(route(spent, BOUND, true, true), Route::Repair);
        assert_eq!(route(spent, BOUND, true, false), Route::Rescope);
        // #2572: no findings and budget left is another attempt, not a dead end.
        assert_eq!(route(spent, BOUND, false, true), Route::Review);
        let unproductive = Usage { attempts: MAX_UNPRODUCTIVE_REVIEW_ATTEMPTS, calls: 100 };
        assert_eq!(route(unproductive, BOUND, false, true), Route::Backoff);
        assert_eq!(route(unproductive, BOUND, false, false), Route::Backoff);
        assert_eq!(route(Usage::default(), BOUND, true, true), Route::Review);
        // One attempt that spent more than its share (e.g. a fallback
        // reviewer re-ran every group) cannot buy a second full attempt.
        assert_eq!(call_cap(BOUND), 148);
        assert_eq!(route(Usage { attempts: 1, calls: 74 }, BOUND, true, true), Route::Review);
        assert_eq!(route(Usage { attempts: 1, calls: 75 }, BOUND, true, true), Route::Repair);
        assert_eq!(route(Usage { attempts: 1, calls: 150 }, BOUND, false, true), Route::Backoff);
    }

    /// #2572 (PR #2553 / #2387, PR #2545 / #2424): review attempts that never
    /// produced a verdict (evidence budget, recovery, capacity interrupts)
    /// left nothing to repair, yet counted as exhausted rejections. The binding
    /// then backed off forever on unchanged code until the workflow retired
    /// with a green, never-reviewed draft. Without recorded findings, another
    /// attempt is admitted while the binding's call budget remains.
    #[test]
    fn unproductive_attempts_do_not_strand_a_binding_in_backoff() {
        // #2387: two aborted attempts, 26 calls, no findings.
        assert_eq!(route(Usage { attempts: 2, calls: 26 }, BOUND, false, true), Route::Review);
        // #2424: one aborted attempt that overspent its share (77 calls).
        assert_eq!(route(Usage { attempts: 1, calls: 77 }, BOUND, false, true), Route::Review);
        // Still bounded: the call budget and an attempt ceiling both stop it.
        assert_eq!(route(Usage { attempts: 1, calls: call_cap(BOUND) }, BOUND, false, true), Route::Backoff);
        assert_eq!(route(Usage { attempts: MAX_UNPRODUCTIVE_REVIEW_ATTEMPTS, calls: 10 }, BOUND, false, true), Route::Backoff);
        // A recorded rejection keeps #2451's cap: repair, never re-review.
        assert_eq!(route(Usage { attempts: 2, calls: 26 }, BOUND, true, true), Route::Repair);
        assert_eq!(route(Usage { attempts: 1, calls: 77 }, BOUND, true, true), Route::Repair);
    }

    /// The durable store applies the same rule across ticks: the #2387 shape
    /// reaches a third review instead of a permanent backoff.
    #[test]
    fn store_admits_a_review_after_aborted_attempts_without_findings() {
        let (_dir, mut store, lease) = fixture();
        let key = binding_key("21b7c7acac54a8112888692ed547da5b4fefe92d", "41a29214");
        for tick in 0..MAX_REVIEW_ATTEMPTS_PER_BINDING {
            assert_eq!(store.admit_review_binding(&lease, &key, BOUND, false, true, 10 + i64::from(tick)).unwrap(), Route::Review);
            store.charge_review_binding_calls(&lease, &key, 13, 10 + i64::from(tick)).unwrap();
        }
        assert_eq!(store.admit_review_binding(&lease, &key, BOUND, false, true, 20).unwrap(), Route::Review);
        let mut routes = vec![];
        for tick in 0..6 {
            routes.push(store.admit_review_binding(&lease, &key, BOUND, false, true, 30 + tick).unwrap());
        }
        assert!(routes.contains(&Route::Backoff), "unproductive attempts stay bounded: {routes:?}");
        assert!(store.review_binding_usage(&lease, &key, 40).unwrap().attempts <= MAX_UNPRODUCTIVE_REVIEW_ATTEMPTS);
    }

    #[test]
    fn binding_key_ignores_volatile_verification_evidence() {
        // Same head and contract always map to one key; either part changing
        // gives a new one.
        assert_eq!(binding_key("h", "c"), binding_key("h", "c"));
        assert_ne!(binding_key("h", "c"), binding_key("h2", "c"));
        assert_ne!(binding_key("h", "c"), binding_key("h", "c2"));
        assert_ne!(binding_key("ab", "c"), binding_key("a", "bc"));
    }

    fn evidence(head: &str, contract: &str) -> crate::autoship_review::ReviewEvidence {
        let binding = serde_json::json!({"head":head,"base":"b","environment_id":"env","contract_hash":contract,"evidence_hash":"ev"});
        let finding = |id: &str, claim: &str| serde_json::json!({"id":id,"criterion_id":"once-per-local-date","evidence_hash":"x",
            "claim":claim,"requested_change":format!("fix {id}"),"conflicts_with":[],"repair_rounds":0});
        let actor = serde_json::json!({"provider":"codex","backend":"native","model":"m","session_id":"s"});
        let pass = |role: &str, findings: Vec<serde_json::Value>| serde_json::json!({"binding":binding,"actor":actor,"role":role,"completed":true,
            "coverage":{"binding":binding,"required_paths":[],"supplied_files":{},"prompt_hash":"p"},"findings":findings});
        serde_json::from_value(serde_json::json!({"binding":binding,"builders":[],
            "passes":[pass("qa",vec![finding("zone-key","delivery key is (zone, local_date)"),finding("producer","UFCS evades scan")]),
                      pass("diff",vec![finding("zone-key","delivery key is (zone, local_date)")])],
            "adjudication":{"binding":binding,"actor":actor,"finding_fingerprints":{},"prompt_hash":"p","response_hash":"r","rulings":{},
                "components":[{"binding":binding,"actor":actor,"finding_fingerprints":{},"prompt_hash":"p","response_hash":"r","rulings":{
                    "zone-key":{"decision":"uphold","evidence":"e","repair_criterion":"make delivery identity stable"},
                    "producer":{"decision":"dismiss","evidence":"e","rationale":"out of scope"}}}]}})).unwrap()
    }

    /// A capped binding is repaired against what reviewers already said about
    /// it: upheld findings once, dismissed ones never, other heads never.
    #[test]
    fn recorded_findings_come_only_from_the_same_binding_without_dismissed_findings() {
        let review = evidence("6b52b713", "e32b4a6a");
        let text = recorded_findings(&review, "6b52b713", "e32b4a6a").expect("upheld finding is actionable");
        assert!(text.contains("make delivery identity stable"), "{text}");
        assert_eq!(text.matches("zone, local_date").count(), 1, "duplicate pass findings collapse: {text}");
        assert!(!text.contains("UFCS"), "dismissed finding must not drive a repair: {text}");
        assert_eq!(recorded_findings(&review, "ffadea98", "e32b4a6a"), None);
        assert_eq!(recorded_findings(&review, "6b52b713", "025dcb47"), None);
    }

    /// Dispute → amend → review → dispute is bounded per source and original
    /// base; neither an amended contract nor a new head reopens it.
    #[test]
    fn adjudication_rounds_are_bounded_per_source_and_base() {
        let (_dir, mut store, lease) = fixture();
        let key = adjudication_round_key("issue:2329", "c92cffe3");
        for round in 0..MAX_ADJUDICATION_ROUNDS {
            assert!(store.admit_epoch_attempt(&lease, ADJUDICATION_ROUND_PURPOSE, &key, MAX_ADJUDICATION_ROUNDS, 10 + i64::from(round)).unwrap());
        }
        assert!(!store.admit_epoch_attempt(&lease, ADJUDICATION_ROUND_PURPOSE, &key, MAX_ADJUDICATION_ROUNDS, 20).unwrap(),
            "third adjudicated amendment must rescope instead");
        assert_ne!(key, adjudication_round_key("issue:2329", "other-base"));
        assert_ne!(key, adjudication_round_key("issue:2350", "c92cffe3"));
    }
}
