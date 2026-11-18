# Native family-budget admission fence

Part of #1703 / #1706; a prerequisite for T's #1799 delegated command admission. This slice persists family membership, explicit policies and generation-bound funding intent, and fences the real native model-charge boundary. It does not activate delegated jobs or complete either issue.

## Authority and activation

T must authenticate the current owner/repository authority, original command scope, child publication receipts and explicit finite policy before using these internal Rust APIs. A stored reference string is not authentication. There are no numeric or environment defaults for family limits. Compatible daemon and worker readers and current execution custody are prerequisites for T activation; a new table cannot make an old reader enforce it.

`WorkflowStore::with_family_transaction` provides one IMMEDIATE transaction for trusted family and child-admission metadata. An error rolls back the entire batch. T must commit pending lineage before public publication, then use a later all-child transaction for verified membership, manifest and compatible admissions. These APIs neither create public issues nor authenticate command requests.

The transaction helpers live in `autoship_family_budget_store.rs`: `begin`, `stage_member`, `finalize`, `activate_policy` and `record_intent`. Membership and policy writers require a current root lease and expected root binding. Begin replay also revalidates the binding. Intent is immutable within a child/root lease generation; changing policy or resuming after a root-control revision requires a fresh verified frame after normal custody checks.

## Accounting

The existing resource-charge ledger and native admission receipts remain the only spending source. Joining retains parent and child charges. Each canonical member is counted once, and positive admission charges require exact native receipt attribution. Missing or contradictory history yields unknown verified totals while reported member totals remain visible. Unknown never becomes zero.

Version-one policies supply finite positive model-admission and committed-wall limits. Wall units are the existing persisted controller elapsed deltas, not a new run timeout. Token completeness is not established by the current admission producer: token-limited policies are unsupported, and reported token completeness remains unknown.

A new admission is checked and charged in the same IMMEDIATE transaction. Two child workers cannot both spend the final admission. An existing identical receipt can still be recovered after exhaustion or pause; this is not another dispatch grant. Late wall/token accounting remains recordable. No family operation debits merge allowance or fabricates a parent merge receipt.

A dispatch has one original command funding origin. Protected Independent intent preserves a proven pre-existing child's grant. Protected Delegated intent identifies the original command's family, manifest and policy; missing intent cannot silently choose independent funding. Passive membership in another family does not grant or replace authority. Observed member charges remain shared historical facts for later family observations.

## Delivery boundary

The read API supplies the typed budget-only observations documented in [family-budget-status.md](family-budget-status.md). It is not a lease, authority grant or complete lineage-readiness result. T/N still need to compose and project these facts through actual command/workflow status; this read API alone does not persist an exhaustion checkpoint or activate a job. T still owns authenticated policy production, lineage recovery, command admission and status composition. Pending incomplete publication stays held until verified T reconciliation; no missing child is replaced to escape the fence. E's ordinary-issue-to-merge and live fault/soak proofs remain separate acceptance gates.

Tests use temporary real workflow stores and the actual `charge_admitted_model` entry point. They launch no provider and grant no production quota. Coverage includes pending/missing policy, retained spend, concurrent final-slot admission, restart, owner pause/resume, current root binding, immutable policy/scope, unknown/orphan attribution, late accounting, unsupported token policy, batch rollback and unchanged merge allowance.

## Controller control checks

`autoship_family_controls.rs` shares the exact generation-bound frame reader with native model admission. Its control-only check verifies the funding/lineage/policy references and ancestor control revision for a new publication mutation or native launch. An independently authorized existing child keeps its original grant despite passive membership in a paused family. Compute exhaustion or unknown aggregate spending does not revoke verified control for an unpaid action; the unchanged positive-admission predicate still refuses paid work with exhausted or unknown spending. A control-only success is not a budget-Ready observation, authentication or a new execution grant.

The installed native dispatch observer and controller publication intent, bound push, note/ready and publication-reservation entry points consume this check. S's durable operation/push/publication reservation transactions recheck it as well. Refusals enter the existing specific durable wait path before any model charge. The old controller's hold flag also fences native dispatch, even if the control-only predicate remains ready after compute exhaustion. General lease/binding and shared metadata-recovery checks remain separate; no family control check was added to completed-effect receipt or late-accounting methods.

M's final merge reservation/dispatch transactions still need the same family-control predicate beside their existing owner-control guards; this S slice does not edit M's implementation. Automatic T funding-frame production, ancestor cancellation/teardown composition, and ordinary live acceptance remain separate integration work. The family feature is not activated by these component tests.


## Post-pipeline wait settlement

Ordinary issue, resumed PR and adopted-PR execution settle a confirmed family wait after their pipeline future returns. Settlement retains the original controller logger; it does not reconstruct an empty process registry after restart. It signals only that native owner's supervisors and waits up to 30 seconds for their actual cleanup registrations and checked audit drain. Missing cleanup proof leaves execution custody fenced.

A fenced store transaction revalidates the live lease, active same-generation wait and exact registered audit proof, records native retirement intent and releases the execution lease. The checkpoint, stage, evidence, meaningful-progress clock and five-minute observation deadline remain intact. Actual elapsed usage is recorded; model admissions and merge allowance do not reset. A shared family-settlement marker prevents guard destruction or completion from repeating release or charging after it. Existing non-family completion behavior is unchanged.

Failed wait persistence cannot become a successful held report or a cleanup release. An idle-recovery request still requires its separate recovery completion. Native VM retirement retains its existing checked retry path; releasing a stopped controller is not a claim that retained VMs have already been deleted.
