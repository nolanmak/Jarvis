# Initial acceptance verifier repair (#1943)

Implementation contract under #1706/#1710/#1703, separate from capacity C0.
This is a draft until review completes. It must repair the production #1937
acceptance failure without weakening independent baseline red, enrolled criteria,
source/resource/input authority, review/CI or confirmed-merge accounting.

## Evidence and scope

The production PR1937 command was accepted and its first worker attempt failed
with acceptance_registration_retry after four model admissions. Verifier
roundtrip-bound failed its own candidate-size setup assertion, producing
UnexpectedFailure, not intended red. Current prepare_acceptance_for already
tries initial_baseline_correction once. A failed correction then reaches generic
five-minute retry; completed model output is durable, but no typed, immutable
baseline-observation record coordinates subsequent behavioral correction.

BR1. Apply one shared coordinator to initial independent acceptance preparation
for fresh issues, linked PR resumes and source-less PRs through the existing
prepare_acceptance_for. Existing valid prepared evidence/manifest selection and
original-baseline recovery precede this path and remain unchanged. Never regenerate
an enrolled original contract, choose candidate HEAD as baseline, drop criteria,
accept UnexpectedFailure as intended red or grant native ownership from a receipt.

BR2. For one immutable repair epoch permit the original proposal and at most two
behavioral corrections (three behaviorally observed candidate slots total).
Existing per-workflow/family model-call/resource budgets still apply; this limit
never increases them. Schema/source-selection retries within each independently
keyed proposal retain scope_authority::persistent's existing completed-response
limits and original model accounting. Provider/infrastructure failures propagate
without consuming an additional completed-candidate slot or bypassing admission.

BR3. Epoch binds a versioned domain, actual workflow identity and current workflow
contract binding, exact AcceptanceSource, trusted original base, verifier
execution environment identity, exact scope bytes and all required criteria in
order. The authoritative environment value is the existing
self_improve_contract::environment_id on that original baseline checkout (currently
sandbox-protected-cache-v2:<SHA-256>), including its existing sandbox/exporter,
dependency audit, compiler-control, lock/toolchain, executable-search and runtime
inputs. Do not substitute current deployment SHA or a wall-clock/sample identity.
Obtain and verify the guarded clean original baseline first, including after
interruption: reset/clean-baseline verification -> environment identity -> epoch
and cache validation -> candidate installation -> observation. Previously
installed verifier files may not influence epoch identity. Canonical serde encoding of that fixed tuple is SHA-256 hashed. No clock,
lease generation, temporary pathname, random directory, error/output digest or
current free-resource sample creates a new repair budget. Changed required
source/base/contract/scope/environment legitimately creates a different epoch;
it never clears an older enrolled registration or unrelated hold/accounting.

The original proposal uses existing proposal_with_retry and its existing caches.
Correction 1 preserves the existing initial_baseline_correction cache identity
for the same source/base/environment/criteria, so an already completed legacy
correction is observed once and counted, not discarded for new calls. Correction
2 uses a separately versioned key binding the full new epoch and slot. No prior
raw response is edited/deleted. Prompts preserve every original criterion and
quote the previous failure as diagnostic data, never instructions/permission.

BR4. Before observing a candidate, reset only the original controller-owned clean
baseline checkout using the existing guarded Git path; install/observe only the
independent proposal against the unchanged original base. The actual
register_and_reproduce executor is the production callback; a scripted boundary
may test coordinator behavior but cannot certify an actual baseline or mint a
production BaselineEvidence. Candidate/source checkouts and owner files are not
reset. All current source/resource/lease and protected-audit checks stay in force.

On true Reproduced, return the original current BaselineEvidence to the existing
prepared-evidence persistence. This new cache never stores a positive permission.
A crash before existing prepared evidence persistence may rerun the original
executor with the cached proposal; it cannot invent positive proof from JSON.

BR5. Persist failed completed baseline observations in the existing private
scope_stage_response store, under a domain-separated key derived from epoch
and slot. Each slot can name only one exact proposal digest, validated from the
record; changing a candidate cannot fork a new key and replenish the slot. Use the existing lease/binding-checked immutable
writer, not a second database or allocator. Record fields are exactly version=1,
epoch, slot (0..2), proposal_sha256, prior_sha256 (null only at slot 0), kind
and private detail. prior_sha256 hashes the exact preceding failed record. Load
all three stable slot keys before generation/observation; failed records must form
a contiguous prefix with matching predecessor hashes. A later node without its
predecessor is unknown before any work. Matching cached failed slots require the
original cached proposal (or valid inline original proposal); cache-only lookup
may not fall through to model generation. Missing required raw proposals hold. Strict decode rejects
unknown/duplicate fields, bad SHA/slot/epoch, oversized data or changed payload.
Encoded data stays within that store's existing 2 MiB bound; actual complete
proposal/digests and all criteria remain untruncated. Diagnostic feedback may
be bounded separately and is JSON-quoted. Bad/missing required stored evidence
is unknown, never a reason to erase a slot or accept a candidate. The store reader
must detect a missing response row when the existing committed response event
records that key; it must not treat such deletion as an unused slot. An absent
trailing slot with no committed response event is legitimately unobserved, so a
crash before persistence can retry observation. Initial legacy adoption may read
pre-existing correction output only after observing/recording its predecessor;
it does not create new failure rows from saved model text.

Kinds distinguish already-passes, not-reproduced and unexpected-failure using
actual BaselineResult/CheckResult, not guessed words in stderr. UnexpectedFailure
may come from a bad fixture or uncontrolled program failure; do not assert its
cause without diagnostic evidence. If any required observation is
InfrastructureFailure, retain the existing infrastructure outcome/backoff and
do not mark the candidate invalid solely from that observation. A parser/source
selection failure remains its existing bounded typed outcome, never baseline red.

The failed observation must be durably recorded before another behavioral
correction can be requested. Replay of a matching failed slot reuses the recorded
outcome/diagnostic without rerunning that baseline or calling a model. A crash
before failed-observation persistence may repeat the original observation, but
not request another correction from an unrecorded failure. Concurrency remains
serialized by the original workflow lease and guarded immutable store writes;
no detached correction task or stale-lease publication is permitted.

BR6. After three invalid completed candidates, return a typed, public-safe
acceptance-verifier-repair-exhausted hold. Preserve the exact private failed
lineage. Do not wrap it as BaselineNotRed eligible for automatic source
decomposition, use it to close an issue, clear owner control, or spend merge
allowance. Malformed, conflicting or missing required evidence returns the typed
acceptance-verifier-evidence-unknown hold, with await-original-evidence-recovery
action. Re-read such evidence unpaid; only restored matching original protected
state or legitimately newly authorized inputs producing a different epoch allow
reentry. No deletion, reset, synthesized record or ad hoc database repair is added.
Completed schema/source-selection exhaustion is also a typed hold;
provider/infrastructure failures retain distinct retries and accounting.

Every current prepare_acceptance_for caller must handle this hold without
rewriting it to generic acceptance_registration_retry. Persist a Scope checkpoint
with stable reason acceptance_verifier_repair_exhausted,
acceptance_verifier_evidence_unknown or acceptance_contract_invalid, as applicable.
The normal exhausted/invalid action is await_acceptance_input_change; unknown
evidence uses await_original_evidence_recovery. Retain current binding/evidence. Repeating the same hold leaves progress_at/events unchanged. Ordinary
unpaid reconciliation may revisit the cached coordinator under its original
workflow guard, but cannot call a model, rerun known failed baseline candidates,
repeat decomposition or report that revisit as meaningful progress. Scheduler
fairness/peer selection and all independent owner/provider/CI holds remain intact.
Changed epoch returns through ordinary authorization/admission, not direct dispatch.

BR7. Full private diagnostics remain available to the original recovery path;
CLI/progress expose the stable type and next action without source paths,
credential contents or verifier output. Existing Discord progress/outbox handles
delivery independently. A retry/hold is not review approval, shipment or deployment.

## Test-first delivery

- Reproduce the existing actual coordinator behavior with a valid original
  proposal and inadequate first correction: the second correction must be
  reachable only after recorded failure, and current one-correction behavior
  must fail that regression before implementation. New-helper absence is not
  counted as deployed behavioral RED.
- Script the model/executor boundaries while using the same production repair
  coordinator and immutable WorkflowStore adapter. Cover wrong setup, inadequate
  correction then real successful observation, all-invalid exhaustion, parse
  exhaustion, infrastructure/provider failure and zero fabricated positive proof.
- Crash/reopen before/after response, failure and existing prepared-evidence
  persistence. Before positive evidence persistence, cached proposal alone grants
  nothing and the real executor may rerun; after it, valid original prepared
  evidence selection wins. Also cover changed epoch,
  malformed/conflicting records, stale/concurrent leases and exact legacy
  correction adoption. Verify no new calls/observations or rewritten responses
  on replay of completed failures, and actual admissions still use old budgets.
- Exercise all fresh/resume/source-less callers' typed hold handling and progress
  idempotence through real control-store fixtures; keep enrolled-baseline reuse
  and criteria-preservation regressions. An unexpected setup exit never enrolls.
- Use scripts/cargo-managed.sh for Rust work, relevant acceptance/scope/store/
  progress tests and hosted CI. Record tested SHA, ignored live prerequisites,
  independent code approval and normal rollout. Re-observe the original #1937
  command after rollout; do not clone/reset it or claim queued work as success.

## Boundaries and review gate

Fresh-reader, source/conversation comparison and complete narrow seam review
precede implementation. This coordinator repairs unenrolled independent verifier
proposals only. It supplies no all-writer/native release, owner acceptance,
retirement, source/resource/input custody, capacity migration or publication
permission. Full #1703 CLI/Discord/merge/fault/soak acceptance remains unfinished.

## Review and behavioral RED record

Fresh-reader, source/conversation comparison and all 31 mapped narrow seams were
checked. Findings about missing-evidence exit, exact environment identity,
reset-before-environment ordering and positive-persistence crash coverage were
resolved/rechecked. No remaining scoped finding was retained. This is contract
review, separate from required independent code approval and live acceptance.

Behavior-preserving extraction commit 2ba400d2 on the #1943 source branch routes
the actual existing initial registration through the same one-correction
coordinator and adds its regression. Managed CLI test runs one case and fails:
a valid original proposal plus inadequate first correction cannot reach the
scripted second independent correction. This is deployed sequencing reproduced
after extraction, not a missing helper and not an actual baseline certification.
