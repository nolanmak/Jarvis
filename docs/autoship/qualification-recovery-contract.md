# Qualification recovery: source data and durable holds

Status: source replay, the unchanged productive-claim fence, original native
custody recovery, original-verifier replay and bounded recovery scheduling are
implemented locally for PR #2035. Final integration validation and the
producer/exit fixes described below still block rollout. This is not
a restoration, full #1952 closure, or permission to merge known defects.
Parent issues: #2034, #1952 and #1703.

## Decisions and boundaries

1. Source read selectors are data. A previous native source-selection response
   may supply revalidated immutable Git excerpts to a current independent
   qualification. It must never become a current-policy native judgment or
   final-review approval. This avoids reissuing an already completed original
   source-selection request without weakening current qualification policy.
2. The first compatibility recipe is pinned to main `7353e9cd`: scope policy
   `bd44467c5c2ddedd493a2c8ce392578669b5ec8570b622403d05d92625f20771`,
   qualification policy
   `7a16a5c6b515bdf9e065f955bcc1e436de04a692e29d764bd5f89b3979e1b998`.
   Rebuild the old qualification context from the exact proposal and measured
   baseline, then rebuild its original prompt. In the incident this reproduces
   both saved context and parent-request hashes, verified read-only against the
   original evidence. No arbitrary old policy is accepted.
3. A controller-owned `SourceReplay` record binds current epoch/request to the
   exact original input, provider, epoch, parent request, known policy and
   source-choice request hash. Creation verifies the complete original native
   input hash, original private provenance and retained admission custody.
   Consumption reads exactly that input, checks the source-selection role and
   every original field again, and revalidates selectors against current reads
   of the same immutable Git base. Original response/provenance rows stay
   unchanged. The record uses existing immutable scope-response storage; it
   does not copy a model response under a new native-response identity.
4. Current native witness replies and qualification artifacts retain current
   policy binding. Compatibility applies only to source selectors. Different
   workflows, original requests, roles, providers, source bases or unrecognized
   policies do not acquire compatibility through a shared parent epoch.
5. A typed qualification hold on enrolled work preserves its candidate binding,
   registration, original evidence, private diagnostics and expenditure. The
   initial-enrollment fence stays strict. Actual paid admissions determine
   billing through existing capacity accounting.
6. An in-memory builder refusal is insufficient. After guard release, unchanged
   productive claims for original-evidence and verifier-infrastructure recovery
   must be fenced. Adding only a permanent fence is also insufficient: verified
   recovery must reopen the same request through the existing unpaid
   reconciliation and claim authority, without resetting budgets or creating
   another work queue.

## Source-replay implementation and tests

The qualifier builds both request contexts from its immutable prepared record.
`prepare_source_recovery` publishes only a small reference projection. The
native scoper consumes it before generating source selectors; its eventual
judgment is still a fresh/current-policy response. A correction with a different
request does not silently inherit an incompatible projection.

The source-data projector follows the original bounded two-response selection
protocol separately for each provider. It first binds the initial response to
the exact original native input. If immutable Git validation rejects it, the
controller reconstructs the exact correction request from that rejection and
the original response, then matches the retained correction's complete native
input and provenance. Only an accepted selection is projected. An unrelated
correction sharing the parent context, or a correction without its original
rejected response, cannot substitute. Fixed prompt-hash vectors extracted from
main `7353e9cd` prevent the fixture and implementation silently drifting to a
different reconstruction recipe.

Tests retain the direct-current-policy rejection and exercise the explicit
source-data path separately. Required negatives include another workflow,
another original request, another current request, a witness rather than source
role, unknown old policy, changed/missing provenance and mismatched native input.
Actual parent/context hashes from the incident were reproduced without writing
production state or copying the database.

## Durable recovery integration still required

Owner: the normal workflow controller. Trigger: before #2035 can merge.

- The existing productive-claim fence now covers the two recovery holds. A stale
  preview or a new owner message alone cannot replace missing original results.
- Native custody uses the protected descriptor and atomic transitions described
  below. Infrastructure uses the original-plan and result records below.
  Preserve the failed binding and append-only history; do not manufacture a
  changed Git input to force a retry.
- Recover original native results only after their exact original admission,
  response and private provenance are complete and positively validated. A
  completed semantic `unknown` judgment is different from missing custody:
  physical file availability must not clear a semantic unknown.
- For verifier infrastructure, reuse the exact original pinned measurement
  operation and its existing resource/storage admission. A generic provider,
  memory or disk observation is not proof that its particular failing operation
  recovered. The unpaid recovery path must structurally refuse all model calls
  and preserve unknown effects, retained sessions and original cleanup duties.
- Positive original-evidence or original-operation recovery can return work to
  normal qualification/correction. It supplies neither intended red, final
  review nor merge approval on its own. Unchanged or unknown recovery remains
  a precise private hold, with no productive reclaim loop.
- Reuse the existing five-minute reconciliation cadence. Bound and fairly
  rotate inspection work so one unresolved request does not block peers or
  final merge reconciliation. Inspection does not create productive progress.

The native-custody path below does not complete infrastructure recovery. The
original-plan executor, result handoff and scheduling have component coverage
below; raw-execution and measurement-persistence producer coverage remains open. They
must be resolved in code and tests; they are not requests for an owner-selected
limit or a permission gate. Full #1917 host sizing, productive CLI/Discord and
#1703 live/soak acceptance remain separate requirements.

## Implemented original native-custody transition

The qualification controller classifies a physical private-custody failure or
an unresolved original native admission separately from a semantic unknown.
The existing checkpoint transaction records the hold, its cause and an immutable
descriptor in existing scope-response storage. The descriptor binds workflow,
failed lease generation, complete checkpoint and the exact original native
admission-history digest. A failed descriptor write rolls back the hold;
previously committed original admissions and their charges stay intact.

The existing productive claim can inspect this descriptor without a model or
subprocess API. In the same claim transaction it requires the original cause,
checkpoint, generation and admission history; complete accounting and original
response/provenance pairs; and the normal private-file custody validator.
Owner pause, active lease, pending mutations and uncertain merge dispatch still
block claiming. Missing or invalid evidence yields no claim and no new attempt.

A positive inspection changes the next action to
`original_native_custody_recovered / requalify_original`, preserving binding,
evidence and accounting. The scheduler's passive transaction grants no lease
and changes no attempt or generation. The existing productive-claim path can
also validate custody and claim a lease in one transaction. Both paths produce
recovery-only authority: normal current-policy qualification is still required
before a builder can run. If the worker dies after either transaction, the next
normal claim resumes requalification. If the transaction itself fails, the
original hold and descriptor remain available; recovery was not consumed.

A semantic unknown never creates recovery authorization. A later semantic
unknown in the same lease invalidates the latest physical-cause classification;
a later generation cannot consume the previous generation's descriptor. No
file restoration, owner message or stale descriptor can clear those holds.
Original results and descriptors are retained through the existing store's
lifecycle; this path neither resets budgets nor retires retained sessions.

Tests cover the actual private validator and admission observer, unchanged
failure while the proof is missing, exact restoration with one original model
admission, semantic unknowns, crash resumption, concurrent claims, paused work,
and injected transaction failures. Store-only transaction tests explicitly
script filesystem validation; they do not claim live model acceptance.

## Implemented original measurement-input retention

Before invoking the original executor, the existing bounded initial-verifier
coordinator commits an immutable measurement intent containing its epoch,
correction slot, proposal digest and prior failure-record digest. The intent
contains no copied model response, successful measurement or approval.
Infrastructure failure retains this input identity without consuming a semantic
correction slot.

On resume, the coordinator validates the complete failure prefix and all
intents before any proposal/executor callback. A retained intent forces the
existing proposal producer into its cache-only mode. The original cached
proposal must still be available and match its saved digest; missing or changed
data cannot become a replacement model request. Legacy failed prefixes continue
to use their existing proposal-digest validation without rewriting old records.

Intent persistence must succeed before execution. Failure-result persistence
must succeed before another semantic correction. Tests distinguish these two
boundaries and cover process exit before/after intent persistence as well as
existing response, failed-result and unpersisted-success boundaries.

This is input retention, not a replay permit. The original executor and
inspection-only replay below bind it to the actual measurement. Unknown-result
settlement and scheduling are implemented below; their remaining validation
and producer/exit requirements still block rollout.

## Implemented unpaid infrastructure-inspection boundary

The existing reconciliation claim recognizes a verifier-infrastructure hold
inside its claim transaction. It records `qualification_recovery_claimed` for
the exact new lease generation without incrementing productive attempts.
An original raw-verifier evidence-unknown hold is inspectable only when its
controller-retained plan classification matches the exact checkpoint. This
does not make an unrelated semantic unknown inspectable or recoverable.
Claims are spaced at least five minutes apart for the same workflow/input
binding, including across restart. Genuinely changed inputs do not inherit an
older operation's retry delay. Owner pauses, pending mutations, uncertain merge dispatch and
unresolved original native outcomes prevent the claim. Physical native-custody
holds use their read-only positive check above rather than fencing the missing
original outcome with another inspection lease.

This lease generation cannot admit a model through attributed, bounded or
generic new model-charge paths. A reused original receipt cannot authorize
another actual dispatch. Idempotent historical accounting and new zero-model
wall/resource usage remain recordable. Publication intent, publication dispatch
and merge reservation/dispatch enforce the same inspection restriction.

Ordinary raw-command dispatch retains the existing owner/lease check and fresh
resource/storage admission: blocking every command would also block the
original verifier needed for recovery. No checkpoint edit or process restart
can remove the generation's model/publication restriction. It ends only with
that fenced lease; a later ordinary productive claim for a real changed input
uses the normal admission rules and original cumulative budgets.

The original executor now retains its complete proposal, base, environment and
executor identity before constructing the measurement future. The supervised
command admission callback records the actual kernel scope before release;
completion records its matching quiescence proof. Before another measurement,
every earlier admitted scope in the workflow must be proven quiescent, including
scopes from a different proposal or candidate binding. Missing or changed
admission evidence blocks execution. Infrastructure holds retain the exact plan
in the checkpoint transaction; a later semantic hold clears that classification.

An isolated delegated-service test exercised a live command, refusal to call
its live scope quiescent, and successful quiescence after exit. This verifies
the existing kernel boundary, not an autonomous workflow or a recovery replay.

The executor now retains its actual registered result before classifying red,
non-red or infrastructure failure. Conclusive original results re-enter the
normal measurement coordinator without another command. Exact original-plan,
registration, command admission and kernel-quiescence evidence are checked;
missing, altered or conflicting results remain unknown. A conclusive non-red
result feeds the original bounded correction slot, never acceptance success.

The inspection replayer loads only the held protected plan and requires the
same workflow binding, original base, environment and executor identity. It
uses the existing isolated raw executor and normal storage/resource admission,
without a native-model runtime owner. Its transaction hands a retained result
back to ordinary qualification or correction while preserving binding, evidence,
charges and productive attempts. The inspection generation remains unable to
admit models or publication after that handoff. Failure to retain the recovery
event rolls back the checkpoint transition.

Resource-backed raw verifiers now use the existing durable CI cleanup hold for
their supervisor receipts, source, runtime and target. If the controller exits,
normal cleanup and build admission require verified original scope quiescence.
The raw child receives no inherited storage-lock capability. A real delegated
kernel test confirms that losing the hold object while the child is live keeps
cleanup fenced until that exact scope is empty. Unprotected fixtures without a
resource declaration retain their existing non-resource execution path.

The trusted supervisor now captures bounded stdout/stderr and terminal status
outside both worker writes and the controller's output pipes. It syncs the
streams before publishing an exclusive, atomic manifest after descendant
reaping. The manifest binds the original command and actual resource identity,
with stream lengths/hashes and explicit completion/interruption status. The
capture capability is removed from child environments. Capture storage checks
private ownership, original directory device/inode, request identity and all
output hashes before returning any observation.

Quiescence without an original outcome no longer permits another attempt.
Complete captured commands resume the same attempt and reconstruct their
registered receipts; only never-started verifiers execute. A recorded actual
interruption permits a new attempt after quiescence and normal admission,
without consuming a semantic correction slot. Its manifest references remain
in protected history. Missing or changed evidence stays unknown; no clock,
checkpoint edit or model replacement manufactures its outcome.

When a completed verifier has an interrupted peer, the successor attempt keeps
the completed command's original identity. An optional `command_sources` map
in the protected result binds each reused verifier to its original attempt;
records without the map retain their existing same-attempt meaning. Both the
original admission/quiescence and complete private capture/output digest are
revalidated. A reused verifier cannot create a new capture or command admission.
No old admission is copied under the successor's identity. Only interrupted or
never-started peers execute, whether the interrupted aggregate result was
retained or lost. Multiple completed source attempts for one verifier remain
unknown rather than selecting an arbitrary observation. The mixed-outcome
regression covers both aggregate-retention cases and deletion of the original
admission; it failed before this fix.

Real delegated-service tests cover fresh execution, loss of whole-result
retention after actual capture, and partial completion of a two-verifier recipe.
They verify original attempt/receipt preservation and zero model calls. Actual
subprocess tests separately kill the owner process and verify interrupted
capture plus child reaping, including cancellation before child release.

Verifier cache names now include storage's canonical Git common-directory
identity. Worktrees share their repository's cache; identical contracts in
different repositories cannot collide or bypass ownership checks (#2036).

Recovery is not ready for rollout. The scheduling integration below is local;
these component tests do not prove unattended selection, independent
qualification, publication or merging.

## Local scheduling integration

The supervisor runs recovery separately from building, merge reconciliation and
review canaries. A dedicated owned controller checkout refreshes repository
identity, write permission, source authorship, lifecycle, source issue and PR
dependency before dispatch. Issue exclusions and workflow owner pauses remain
effective. Recovery uses only the retained registered candidate worktree and
its exact branch, head, contract, environment and evidence. An unpublished head
also needs the existing retained-candidate record and an ancestor check against
the current published PR head.

Selection keeps at most four inspections in flight and fills vacancies every
five minutes. Active workflow IDs are excluded even while source lookup runs
before a workflow lease exists. Its protected
event records the binding and selection time before remote lookup, so failed
lookups rotate too. Selection orders by the oldest last-selection time for that
binding, then workflow ID; each pass examines at most 128 rows. A selection
event imposes a five-minute cooldown on the same `(head, contract,
evidence_hash)` tuple across process restarts. It is not an execution lease and
does not need a separate release; after the cooldown, a still-due idle hold may
be selected again. Events compare the three binding fields individually;
serialized JSON object ordering cannot bypass the delay. Selection neither
claims a productive attempt nor records productive progress. Each inspection
has a 45-minute bound including source lookups and admission waits, and a raw
replay still uses current resource/storage admission. Native
custody inspection allocates no native runtime. Routine upgrades stop new
selection and drain admitted inspections; operator shutdown retains the
existing guarded cleanup and durable capture responsibilities.

The private `qualification_execution_context` table preserves the original
frontend request and its shape separately from registered contract metadata.
Checkpoint and execution context commit atomically. Public progress does not
include the request. A restarted productive guard reuses this context only for
the same owner request and retained worktree. Positive inspection queues the
existing normal qualification/correction selector; its wake hint does not
authorize execution. Recovered work may pass the old daily-refusal selection
filter, but all productive claims, model accounting, current qualification,
independent review and merge requirements remain mandatory.

Existing frontend-bound workflows acquire execution context through their
ordinary authenticated resume, before binding registration metadata or writing
a new recovery hold. The compatibility regression starts with the deployed
`reconcile / resume_started` shape and no context table, then checks successful
context capture and an unchanged-input fence after an enrolled hold. The
November 12 read-only inventory found zero existing recovery holds, zero native
physical classifications and zero raw recovery descriptors; these authorities
are introduced by this PR. #2014 is paused at that frontend-bound resume shape,
and its current issue/PR request hash exactly matches its checkpoint contract.
It does not need a legacy-context migration. No database backfill is introduced.
Missing or invalid context for an unsupported recovery record remains held;
current issue prose never manufactures historical authority.

## Raw-command authorization and descendant confinement

The original replayer supplies a private, non-deserializable `OriginalExecution`
to the registered executor. `require_registration` checks original base,
environment, complete contract and pinned adapter bytes before materialization
and command iteration. Commands come from that registration's typed invocation;
the release observer records only a verifier present in the frozen plan and
binds its plan/attempt/verifier to the actual kernel admission and private
capture. The shared raw launcher supplies supervision and resource admission;
it is not a substitute for those earlier exact-recipe checks.

The raw child starts with a cleared environment, a separate empty home and
explicit read/write roots through `scripts/codex-command-sandbox.py`. Linux
Landlock, `no_new_privs` and seccomp restrictions inherit across descendant
execution. Socket creation/connection, process introspection and namespace or
cleanup-group escape are denied. The actual compiler-descendant test
`isolated_cargo_finds_operator_compiler_and_preserves_confinement` verifies
private reads, writes and network access are refused. Frozen-registration tests
reject changed base/environment/adapter bytes; inspection-store tests reject
new audited model admissions and publication. These are different boundaries:
neither resource accounting alone nor the model-admission fence supplies the
filesystem/network confinement.

## Producer/exit inventory

Recovery is owned by the existing workflow controller. Every row preserves
historical admissions, charges and unknown effects; a retry never supplies
acceptance or review approval. The new raw-error and completed-result
persistence regressions failed before their fixes; final validation is pending.

| Producer / exit | Retained fact and classification | Permitted next action / verification |
| --- | --- | --- |
| Proposal or intent persistence fails before raw dispatch | Existing cached proposal and any committed intent; no new command admission | Existing coordinator requires the original cached proposal on intent resumption. `original_measurement_intent_is_durable_before_executor` and `infrastructure_resume_only_reads_the_original_proposal` cover the boundaries. |
| Native result/provenance is physically unavailable | Exact original admission history and physical-cause descriptor; evidence-unknown hold | Validate the original pair/private custody, then queue current qualification. Semantic judgments cannot use physical restoration. Native-custody transaction and same-generation invalidation tests cover both paths. |
| Native judgment or contract validation is negative/unknown | Original reply, provenance and private diagnosis; no physical-cause grant | Existing normal qualification or independent bounded amendment decides the outcome. Restoring files never turns a judgment into approval. Source-compatibility tests retain strict current-policy judgment checks. |
| Raw resource admission or executor fails after plan retention | Original proposal/base/environment/executor plan; typed resource/IO failures become infrastructure holds, unclassified failures become evidence-unknown holds | Inspect that exact plan with zero model calls. Typed holds and conclusive `BaselineNotRed` results keep their existing classification. `raw_measurement_failures_retain_the_plan_and_enter_a_durable_recovery_hold` covers an actual ENOSPC error and an unclassified outcome. |
| Controller exits after command admission | Actual kernel admission, capture reference and cleanup responsibility | Reconcile original scope and captured terminal manifest. Quiescence without an outcome remains unknown. `quiescent_original_without_result_cannot_start_another_attempt`, changed-capture/scope negatives and delegated recovery tests cover this boundary. |
| Captured command was interrupted | Verified terminal interruption plus exact scope quiescence; original attempt retained | Only explicit interruption can permit a new raw attempt of the same plan. Completed peers are reused. `captured_original_resumes_its_attempt_and_interruption_is_explicit` and the partial two-verifier delegated test cover this. |
| Completed raw result is conclusive | Original registered receipts plus each admission and quiescence proof | Reuse without another command. Intended red queues current qualification; non-red queues existing bounded correction. `retained_original_verifier_result_reenters_without_executing_again` and the no-native-owner handoff test cover both. |
| Completed measurement cannot persist its file/index | Original plan and raw registered result remain protected; evidence-unknown hold retains the plan reference | Restore storage and reuse the original result during unpaid recovery. `completed_measurement_storage_failure_retains_its_recovery_plan` injects an index-write failure and verifies handoff with one attempt and zero model calls. |
| Recovery handoff persistence fails | Result and original hold/descriptor remain; checkpoint update rolls back | Retry free inspection after the normal delay. Native and original-result transaction-failure tests check rollback. |
| Source/owner/branch authorization fails or inspection times out | Existing hold, source binding, selection event and any admitted cleanup obligations | No new model or publication authority. The selection cooldown survives restart; active work is excluded and admitted raw children retain durable capture/cleanup. Request reconstruction, targeted authorization, owner-pause, bounded-window and supervisor-owner-loss tests cover the separate boundaries. |

## Open threads from the fresh-eyes checkpoint

All entries are owned by the normal workflow-controller implementation and block
merging #2035 until resolved and tested:

- Native and infrastructure recovery now have operation-specific protected
  identity and transactional handoff. Raw producer regressions and the existing
  full/delegated suites passed. Finish validation of the mixed completed /
  interrupted peer fix, then complete hosted checks and the final seam audit.
- Original-operation recovery must distinguish inspection, resumption and
  replay. Unknown prior effects must be settled before replay; successful
  cleanup must be positively observed through the existing execution owner.
- Native custody preflights now retain a typed physical cause; semantic unknowns
  cannot consume their descriptors. The inventory above defines the required
  infrastructure producer/exit coverage.
- Historical admission evidence never authorizes new resource-consuming work.
  Original-result inspection is read-only; a measurement replay still needs
  current resource/storage admission while structurally refusing model calls.
- Native custody restoration returns only to normal current qualification;
  negative/unknown judgments remain governed by that qualifier. Infrastructure
  recovery must likewise supply no positive witness judgment on its own.
- Persisted fair rotation, independent scheduling and the termination bound are
  implemented locally. Finish integration validation and the final seam audit.
- Existing-workflow compatibility and the separate exact-recipe,
  model-admission and descendant-sandbox boundaries are documented and tested
  above. They do not require a second recovery authority or a database backfill.

Pass A checked this document; Pass B confirmed that the no-model requirement,
unknown-effects custody and paid-admission accounting remain explicit. This
checkpoint is not a completed seam audit or an implementation handoff.
