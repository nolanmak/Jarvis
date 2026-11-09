# Review retry lifecycle — working C2 contract

Status: working decisions, **not implementation-ready**. Under #1952 / #1709,
with scheduling #1706, private reporting #1715 and parent #1703.
[Source inventory](review-retry-lifecycle-discovery.md) remains the provenance
for the incident. C1 PR #1954 at 3debdb8d has published-input hosted GREEN;
its original native command is still in acceptance repair. This document
neither changes that attempt nor grants an implementation exception.

## Decisions settled from source inspection

D1. Failed observations have a separate non-approval type. Do not solve retention
by constructing a partial ReviewEvidence and putting it into structured. Keep
IndependentReview.available=false, recorded_verdict=None and approved=false.
A failed-observation field carries facts, never current passes, adjudication,
coverage completion, completed-fragment entries or merge authority.

Reason: both issue-backed resume and source-less adoption call
apply_adjudicated_amendment on structured evidence before unavailable handling.
The amendment validator already checks complete roles/coverage and independent
final authority; do not weaken it. episode::evaluate and current_review_history
also consume structured evidence for distinct current-authority purposes.
A separate type makes a historical observation unavailable to those paths.

D2. Persist each valid observation before resolving its requested dependencies
or issuing observation two. The producer is execute_evidence_group after native
identity, independence, exact binding and response validation, before
GroupReview::observe can error and before recover_sources can refuse. A second
valid but still-incomplete response must also survive GroupReview's bound error.
Malformed responses retain their original diagnostic/identity as invalid
observations, not valid findings. Preserve the original native observation and
response digest; a recovery attempt cannot invent a missing result.

The first packet records the exact original finding data and source provenance
as observed. Later reclassification/resolution is a separate linked fact; do
not rewrite the first packet or imply that a model claim has been proven true.
Capture the response's typed missing_sources and original request digest, role,
actual native identity and exact ReviewBinding. Do not substitute an index,
current worktree bytes or another head for observed immutable evidence.

D3. Historical facts use the existing workflow store/audit ownership. They are
not stored as completed review_fragment or review_recorded payloads and are
not returned by current_review_history. Preserve that reader's owner floor,
exact binding, pending-owner checks and generation boundaries. Add an explicit
historical-facts reader with source/epoch labels; callers cannot deserialize
its result as current ReviewEvidence. Retain facts across owner/head changes,
but do not import historical approval, dismissals or completion.

An unchanged head may supply unresolved historical findings as labelled prior
questions to a fresh authorized reviewer. A changed head requires current
source validation and current observations before a resolution is recorded.
No history event can create a new reviewer identity, native retirement,
released reservation, allowance credit or a current publication verdict.

D4. Check a deterministic-input hold before the whole affected paid pipeline,
not merely inside run_for_subject. Issue-backed resume calls executable
acceptance/repair and then paid QA before independent review. Source-less
adoption can perform scope registration and conflict repair first. Fresh issue
and repair-loop paths need the same admission decision. The queue may perform
bounded read-only reconciliation and then yield to other work; a held command
must not be marked completed or repeatedly consume model admissions.

D5. Separate a stable pre-dispatch input identity from exact review evidence.
ReviewBinding.evidence_hash is built inside run_impl from candidate/caller
source, rubric, controller context, executable acceptance and immutable-write
audit findings. Obtaining a fresh review binding can itself follow paid work.
It cannot be the only key used to avoid that work.

The pre-dispatch identity must be reproducible without model calls from the
canonical repository/workflow, exact candidate/base source identities, original
registered owner contract/revision, owner-input boundary, source-policy and
review protocol revisions, configured reviewer/model/rubric identity and the
recorded dependency request. Record original exact ReviewBinding separately.
A timestamp, lease generation, native session, progress notification or a new
command request ID must not manufacture a new semantic input.

The exact serialization/producers, source availability probes and missing-key
behavior remain handoff gates below. Failure to reconstruct trusted inputs
must preserve a hold, not silently choose a new fingerprint and call a model.

D6. A changed key permits read-only revalidation, not automatic paid retry.
Use typed producer results rather than error strings to distinguish immutable
missing/denied/nonregular/binary/invalid-UTF8 source, unsupported selector/item
parse or lookup failure, evidence/working-set refusal, candidate drift,
provider unavailable, storage failure and native outcome uncertainty.
A missing blob at a fixed head cannot become available merely because a
cooldown expired. A larger host does not increase the fixed review prompt
budget. Unknown/transient I/O cannot establish an immutable missing blob.

| Observed change | Required action before any new paid call |
| --- | --- |
| Timer, notification receipt, duplicate request, same-input lease recovery | Keep deterministic hold; zero additional model admissions |
| Current source/base or registered owner contract changes | Revalidate owner/source authority and the actual failure; preserve historical facts |
| Policy/protocol or reviewer/model/rubric changes | Recompute the affected request under trusted revisions; unchanged impossibility stays held |
| Provider cooldown clears | Reconcile provider/native outcome, then ordinary admission; does not clear an evidence hold |
| Pending owner input | Use the existing owner-turn boundary; no old review authority or automatic spending |
| Lost native response, result publication uncertainty | Reconcile the original observation first, even if other inputs changed |
| Storage/audit unavailable | Refuse publication/new dispatch; missing history is not proof no call occurred |

D7. Durable hold publication, one-wake claim and actual native admission must
be fenced by existing workflow ownership and input revalidation. Concurrent
workers/restart cannot consume two wakes for one held input. Never use a stale
claim or lease expiry as proof of subprocess teardown or an unspent call.
The existing operation table models GitHub mutations (CreatePr/Push/MergePr/
RetirePr/ReadyPr/RetargetPr), not native review observations. Do not relabel a
review call as one of those operations. Identify the real native-dispatch and
reconciliation producer before specifying the transaction boundary.

## Wake terminology and logical transitions

A **revalidation claim** owns one bounded unpaid check of a held input. It is
not a model admission or a cleared hold. A **paid episode** is one execution
of the existing affected pipeline under its normal complete call budgets;
it may contain several actual native calls. “One wake” means at most one paid
episode can be opened from a successful claimed revalidation of the current
input revision, not one unbounded provider permission and not exactly one
model call. Failed probes never authorize a paid episode.

| Logical state and observation | Required logical result |
| --- | --- |
| Held; same inputs and same deterministic failure | Remain held; no paid episode |
| Held; trusted relevant input changed | One lease-fenced unpaid revalidation claim; hold remains effective |
| Revalidation still proves unsupported/oversized/denied evidence | Keep the hold with the precise current blocker; retain original immutable facts |
| Revalidation cannot establish trusted inputs or original result custody | Recoverable unknown/reconciliation hold; zero new model admissions |
| Revalidation proves the recorded blocker resolved and original outcomes reconciled | Record eligibility for one existing execution claim bound to the exact revalidated inputs; not dispatch permission |
| Eligible; owner, source, policy or relevant input changes before final launch | Revoke stale eligibility and return to unpaid revalidation; do not spend from the old claim |
| Eligible; current ownership, exact inputs and all existing guards pass | Open at most one paid episode; actual calls still require final dispatch, native resource and purpose-budget admission |
| Checking/eligible/running controller crashes | Reconcile original claim and any native submissions before claiming another paid episode; lease expiry alone does not clear uncertainty |

A failed or unavailable unpaid probe may be retried through bounded ordinary
reconciliation backoff without creating a paid episode or erasing history.
Repeated status/probe receipts do not change semantic identity. An eligible
record is neither an owner grant nor an exception to the final dispatch fence.
Logical eligibility must be consumed atomically with the existing execution
ownership; the exact schema, native correlation and crash-replay transactions
remain R/P and R/S implementation gates. These logical states do not authorize
creating a parallel workflow engine or a separate spend/release ledger.

## Existing dispatch and uncertainty seams verified

The shared production hook is
self_improve_workflow::WorkflowGuard::install_model_accounting. It installs
AuditLogger::set_dispatch_observer, which calls WorkflowStore::validate_dispatch
against the current lease/binding. validate_dispatch currently checks binding
and owner-turn admission. tool_audit::dispatch_allowed is documented for both
model and verification launches and final supervisor release. Reuse this
boundary for a final hold fence, alongside the early unpaid pipeline check;
adding only scattered caller checks leaves a race after asynchronous setup.
The implementation contract must distinguish allowed read-only reconciliation
from new model/verification launches; a generic bypass flag is not acceptable.

The same hook installs a model observer that creates a UUID model-call receipt
and charges the workflow. Native process_tree::model_call_admitted_in_scope
also records a protected resource_identity through the audit. These are
separate identities; ordering, a provider name or a later native session ID
cannot establish a one-to-one association. The existing observer signature
returns only Result<()>, so it does not currently return that durable call ID
to execute_evidence_group.

native_session::fresh_review_scope creates a new in-memory NativeSession and
returns (result, actual native ID) only on success. Its early `await?` does not
return a reconciliation handle on failure. Lease::Drop marks the in-memory
session uncertain after submission starts or identity is observed; that flag
is not itself durable workflow result custody. Existing native runtime owners
and audit/resource identities must participate in the durable correlation.
Never conclude no spend from an Err or recover uncertainty merely by calling
fresh_review_scope again. Exact API extension and crash-safe result handoff
remain unresolved; do not claim that the in-memory guard closes restart loss.

NativeSession::Drop also marks registered fresh-review runtime paths retired.
native_vm::prune_and_count can later delete unlocked owned state with retirement
and completion evidence. Runtime transcript/export availability therefore
cannot be assumed to outlive the review scope until a later caller persists
its result. Durable observation custody and that existing retirement lifecycle
must be joined explicitly. Preserve normal resource/retirement authority; do
not pin every native runtime forever or count a new facts event as release
proof. This carrying cost blocks the result-custody interface above.

## Behavior-first exits for these decisions

1. Real group adapter: valid finding plus denied/oversized dependency produces
   unavailable, persists the original finding before refusal and spends no
   recovery call. Valid incomplete response two also survives its bound error.
2. Crash/reopen at first result persistence and dependency refusal: original
   identity/finding survives, or uncertainty blocks reissue; no fabricated
   completed fragment, structured review, amendment or publication verdict.
   Include fresh-scope return/drop followed by normal native pruning before
   outer publication: a runtime export is not assumed to remain available.
3. Actual issue-backed and source-less entry paths: same-input resume performs
   zero additional native admissions, including acceptance repair and QA.
   Current owner/head changes preserve historical facts outside approval history.
4. Changed-input matrix: test every row above independently, including changed
   reviewer with still-impossible immutable source and changed head while the
   original native result is uncertain. Two concurrent consumers admit at most
   one authorized wake; same-key restarts cannot reset the budget.
5. Current guards remain mandatory: pending owner input, unavailable audit,
   capacity refusal, exact-head CI, complete independent review and source
   policy cannot be bypassed by a history or hold record.

## Open threads blocking implementation handoff

- R/P: exact native observation/dispatch IDs and reconciliation APIs, original
  result durability and crash ordering. Resolve before defining hold/claim SQL.
- R/E: strict versioned schemas, canonical hashing, aggregate/per-packet bounds,
  storage-growth ownership, conflict handling, original response/source custody
  and historical resolution rules. No truncation or arbitrary cache reuse.
- R/S: every paid entry point and admission chokepoint, including worker issue
  scoping, initial/adopted/resumed/repair loops and owner follow-ups; exact
  earliest trusted source/contract inputs and legacy unknown-state behavior.
- H/N: public/private compatible payloads and sanitization, wake projections,
  delivery lifecycle and actual CLI/private Discord evidence. Delivery is an
  observation, never a hold-clear signal.
- E: exact fixtures/call counts, process crash tests, fresh/source/seam audit,
  implementation RED/GREEN, hosted CI, independent review and normal rollout.

These are engineering obligations owned by the listed existing subsystems,
not a request for the owner to pick machine limits or waive acceptance. This
working contract closes the representation and pre-dispatch placement choices;
it does not claim the remaining interfaces are ready. No benchmark is active.


## Working checkpoint audit

[Design audit receipt](1703-review-retry-lifecycle-design-audit.json) records the
fresh-reader/source passes. A cross-document recording-hook contradiction was
corrected and rechecked; the explicit unpaid-claim/eligibility/paid-episode
terminology was independently rechecked. [All 39 mapped seams](1703-review-retry-lifecycle-seam-map.md)
remain to be judged after their load-bearing interfaces are settled. A map is
not a completed seam audit. The parked schema, native result custody, complete
paid-path mapping and transaction questions still prevent implementation
handoff; this checkpoint does not approve code or claim deployed recovery.
