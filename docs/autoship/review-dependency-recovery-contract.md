# Large immutable review dependency recovery — #1952

Status: investigation/design draft. Not an implementation handoff or activation
authority. Owner #1709; parent #1703. Live incident and original PR identity are
in [the bounded receipt](1703-pr1944-review-dependency-proof.json).

## Observed problem

At candidate3c00c759, self_improve.rs is 967,956 bytes. The review request limit
is 524,288 serialized bytes including system text. Initial review can shard
large evidence, but missing-path recovery reads a full requested blob and
refuses it above that request limit. Repeated unchanged review cannot fix the
controller's representation. Complete CI does not substitute for review.

## Decisions and reasons

D1. Keep the per-request limit and exact head/base/contract/evidence binding.
Do not raise it, clip evidence, turn a source manifest into source coverage,
or ask a builder to alter protected acceptance/reviewer evidence.

D2. Reuse the existing immutable Git source producer and lossless packer.
self_improve_review_callers::Plan::named already resolves a regular tracked
blob, binds its OID/size and streams strict UTF-8 in stable 32 KiB windows.
It rejects binary/invalid UTF-8 and verifies complete reads/child success.
Its 256-entry/8 MiB batch bounds are working-set bounds, not whole-file limits.
The recovery adapter must separately retain valid_missing_path's credential
and control-path exclusions; named does not enforce that policy itself.

D3. One recovery generation may contain multiple bounded source groups.
Every required group must have an actual independent native response. Any
incomplete group keeps the original group incomplete; a third recovery
generation is forbidden. An approve for a part never approves the whole PR.
The original incomplete pass's concrete findings and all recovery findings
remain inputs to existing final adjudication. Source-dependent findings must
retain the exact original and recovered source needed by final authority.

D4. Reuse the existing workflow lease, model admission/resource ledger,
review_fragment cache, three-role controller and final-adjudication machinery.
Do not add another scheduler, provider path, approval flag or storage authority.
A request carries its original group identity and exact dependency ranges.
A restart may reuse only fully validated completed parts for the same binding
and protocol; parent completion requires every planned part, with original
native session independence checked. A missing part is incomplete, not empty.

D5. Reject unknown/denied source and insufficient budgets before the affected
extra call. A real budget or transport failure remains a typed hold with the
original candidate and successful evidence preserved. No command reset,
retained-session deletion or merge allowance debit can repair this failure.

## Existing consumer constraints

- evidence_plan::recover currently returns one Request. GroupReview allows
  two observations, with the original findings retained, and finish requires
  completion. Merely returning the first packed part violates that contract.
- CompletedGroup currently stores recovered_files and finding_evidence, which
  can duplicate source. Persisting a whole large recovery in that object can
  breach WorkflowStore::record_review_fragment's 8 MiB encoded payload bound.
  Source byte size is not encoded size; JSON escaping and repeated evidence
  must be accounted before choosing a durable representation.
- The final adjudicator consumes actual finding source, not only request
  hashes. Its existing bounded source-selection/requested-evidence path must
  receive exact recovered bytes/ranges, preserving supporting and opposing
  evidence. A manifest alone does not discharge that handoff.
- worst_case_calls currently reserves 6*initial_groups+2 for a diagnostic
  canary. reserve_observation is immutable per lease/generation: a second
  different reservation is rejected. Model admission cannot exceed it.
  Dynamic recovery must not silently borrow the shipping budget or report
  partial diagnostic evidence as a completed probe. Normal shipping also
  retains the existing actual per-call admission and workflow budgets.

## Load-bearing open threads

1. Settle the exact bounded recovery-part/cache representation, identities,
   completeness checks and source reconstruction for final authority. The
   8 MiB per-fragment writer bound stays intact; no giant parent payload.
2. Settle total planned call accounting for production and optional diagnostic
   entry points before implementation. Existing immutable whole-probe
   reservation is not an incremental reservation API. All routes must either
   admit their complete declared demand or return the truthful typed hold.
3. Define a stable unsupported-input/budget fingerprint and the existing
   scheduler's wake trigger, so unchanged failure is not repeatedly paid.
   A policy/source/budget change must be able to resume original work.

Owner for all three: #1709/#1952. They block implementation handoff and
activation, not continued incident diagnosis. No schema, source API or runtime
policy has been changed by this draft. Do not treat the proposed grouping as
a proven replacement for complete dependency reasoning.

## Test-first build order and exits

First finish the open contracts and check fresh-reader, conversation/source
and every mapped seam. Then add a scripted-provider test at the production
review/recovery adapter using a real disposable immutable Git dependency above
one request. Record the current exact budget failure as behavioral RED; an
incident log or helper-only test is not this automated RED.

GREEN must prove complete original/recovered byte coverage, every required
role/group, retained findings, exact serialized budgets, original session
independence and existing final authority. Test UTF-8/escaping/exact/+1,
credential and control exclusions, unsupported Git entries, missing/changed
source, incomplete second generation, budget refusal before extra spend,
crash/restart, missing/conflicting cache parts and stale head/protocol.

Run affected checks through the managed wrapper or immutable hosted CI. Code
approval, normal merge/updater rollout and replay of the preserved original
PR1944 command are separate exits. Five autonomous merges, owner CLI/Discord
coding and fault/unattended acceptance remain #1703 requirements. No benchmark
is requested.
