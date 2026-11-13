# Initial verifier budgets across crashes and rollouts

Issue [#2067](https://github.com/nolanmak/Jarvis/issues/2067) extends the
initial-verifier repair under #1943. It prevents an unchanged issue from gaining
another three repair slots after a controller rollout or regenerated guidance.
It does not qualify an original measurement, approve a candidate, or reset any
recorded charge or decision.

## Identity and ordering

The trusted origin consists of the workflow, its protected original frontend
request, original candidate baseline and existing `environment_id` fingerprint.
That fingerprint covers the sandbox/compiler/dependency inputs; the separate
controller-sensitive `verifier_executor_id` is not an origin input. Retry notes,
what merged on main, generated guidance and controller witness-policy revisions do not
create a trusted origin.

The primary fresh-issue frontend saves the complete processed `ScopeOutcome` before
initial verifier preparation. This includes guidance, criteria, complexity and
guard metadata, or an explicit null fallback. Snapshot fields must be present;
missing optional metadata is corruption, not permission to choose a fallback.
The existing immutable scope-response store records the snapshot and its receipt.

After interruption, the frontend reads that snapshot before paid readiness or
scoping. An exhausted initial budget becomes the existing stable exhaustion
hold before either can spend. Otherwise, readiness retains its existing checks
and the saved scoped decision replaces fresh scoping and post-scope mutations.
Original criteria and complexity stay fixed. Enrolled recovery is identified
from the protected execution contract, not `Binding.contract`, which stores the
bare registration hash. It retains its existing source/qualification recovery.

The existing three-slot coordinator receives a pinned accounting epoch through
an immutable budget reference. The reference contains the stable full input
digest, trusted origin, original-plan context, chosen epoch and original policy.
The original-plan context binds workflow, request-contract hash, exact acceptance
source, original baseline and environment. Current reference reads recompute the
epoch with the saved policy. Qualification still uses current policy separately;
historical accounting never imports a positive witness or approval.

The snapshot precedes the reference; the reference precedes proposal and
measurement callbacks. Crashes between these operations replay completed data.
Existing failures, intents, responses, native effects and charges remain intact.

## Legacy custody and bounded inspection

The supported legacy mapping is the producer at main `8d5db4f2`, whose witness
policy is `64507eba1790035881df940099cc1ea640319518572a2a370a310744b883cf40`.
The controller reconstructs that epoch from the exact current inputs and checks
all six failure/intent keys before discovery. A corrupt sole intent cannot be
mistaken for an empty budget. Existing prefix and predecessor validation runs
before callbacks, preserving the original slot and response keys.

When no reference exists, discovery reads the union of protected response rows
and recorded scope-response receipts: 16 keys per page, at most 8,192 keys, and
one bounded response body at a time, at most 2 MiB each. Internal records require
their controller-derived keys. Provider lookalikes do not own an episode.
Unknown or conflicting unreferenced epochs hold before fresh work.

Missing-scope preflight additionally authenticates every retained original plan
and requires a matching reference context. An unrelated reference is insufficient.
It cross-checks typed verifier-index pages with retained plan bodies and ordinary
scope receipts, so losing only the verifier index cannot erase plan history.
Plan inspection is bounded to 8,192 records and the existing 2 MiB body envelope.
Neither inspection executes, installs or settles a verifier command.

A missing scope with a same-origin reference requires recovery even if its
budget is accounted for. A genuinely different owner/base/environment origin
may start fresh only when all prior internal epochs and plan contexts are owned.
Malformed, missing, ambiguous or uncorrelated custody remains a hold.

## Operation and verification

`acceptance_verifier_repair_exhausted` retains
`await_acceptance_input_change`. Missing original custody retains
`acceptance_verifier_evidence_unknown` / `await_original_evidence_recovery`.
Use supported evidence recovery; if no existing path can restore the original
scoped decision, keep the hold and implement a narrowly tested recovery path.
Do not edit SQLite, invent an owner-input change, reset a counter, or manufacture
an approval. This patch adds no automatic resumption of #2014 or #2064.

Behavioral regressions exercise the actual coordinator and immutable store:

- `controller_rollout_after_last_failure_cannot_restart_spent_initial_budget`
  exits a disposable process after its third failure and reopens with new policy.
- `changed_upstream_guidance_replays_original_scope_without_new_scoper_call`
  reopens a processed scoped decision with different future scoper guidance.
- `missing_original_plan_index_does_not_erase_scoping_custody` removes only the
  verifier index while retaining the original plan body and ordinary receipt.

The suite also covers spent-budget preflight, registered recovery, explicit null
versus missing snapshot fields, legacy adoption/refusal, origin changes and
mixed owned/orphan plan histories. Scripted model/executor boundaries are not
production baseline qualification. Use `scripts/cargo-managed.sh` for local
validation; follow [disk storage](../disk-storage.md) and the
[rollout procedure](rollout.md) for snapshots and deployment.

This manually delivered repair earns zero autonomous merges. Original behavioral
red, independent witness qualification, candidate green, exact-head review/CI,
natural autonomous merge, CLI/private Discord and the full #1703 acceptance
requirements remain separate production obligations.
