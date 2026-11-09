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

D3. A proposed recovery generation may contain multiple bounded source groups,
but grouping alone is not a sufficient completion rule (see rejected proposal).
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

## Rejected multipart proposal and fresh-reader findings

The first proposed D6–D9 split the original group plus requested source into
independent recovery parts and combined local approvals. Fresh-reader review
rejected that as a handoff design: every part can approve locally while no
reviewer evaluates the cross-part relationship that caused missing_paths.
Existing final adjudication consumes findings; it cannot necessarily recover
a relationship that no partial reviewer identified as a finding. Complete
byte delivery is not complete dependency reasoning. Do not implement or
promote that multipart proposal. D3 describes a coverage constraint only, not
a sufficient completion rule; D4's reuse constraints grant no such authority.

The fresh-reader pass found six unresolved contracts:

1. A bounded coherent operation must actually evaluate cross-source dependency
   relationships; a union of part approvals is insufficient.
2. Plan/parent identities, references and findings need explicit encoded bounds
   and overflow behavior, not merely the existing per-child writer limit.
3. Wake identity must agree with epoch/binding/admission-counter identity;
   waking into the same exhausted counter is not recovery, and resetting it
   destroys the bound.
4. Lost native-result recovery needs an existing authoritative producer,
   retrieval and validation transition. A phrase such as original positive
   evidence does not name one. No resource-release or retry capability can be
   inferred from lease expiry, handle absence or a cache key.
5. Required findings cannot first hit an unpersistable encoded response after
   the only allowed observation. A pre-call bound or bounded durable response
   representation must preserve useful successful evidence on overflow.
6. Failure classes must specify admitted/result state, cacheability, next
   action and wake trigger. Transport after admission differs from both
   pre-admission quota refusal and a completed-but-incomplete review response.

A separate source comparison also finds that diagnostic runs deliberately
disable normal review-fragment reuse. Any replacement must preserve fresh
actual diagnostic observations; production failure/cache behavior cannot
silently become a diagnostic completion proof.

The seam map enumerated 30 seams for the rejected proposal. These are mapped,
not all checked. No all-seam or implementation-readiness claim is made.

## Next design investigation and handoff gates

Inspect the existing final-authority requested-source/evidence-window producer
before inventing a replacement. It already reads immutable source, reconstructs
exact selected ranges, distinguishes unread text from supplied evidence, and
keeps source-selection work separate from rulings. Determine whether it can
supply a coherent bounded dependency context to the original reviewer.

This is a candidate direction, not a settled solution. The review protocol
currently requests exact paths without a structured dependency question.
Settle how a question/criterion and original evidence guide selection, how
supporting and opposing source are retained, and what happens when necessary
coherent context still cannot fit. A selector/index never independently
approves code or proves the reviewer saw omitted bytes. An unresolved real
dependency remains incomplete with an explicit scope/policy recovery route.

Preserve the existing one-recovery-generation/structured-correction rules
unless a separately reviewed amendment defines a replacement. Count selector
and reviewer calls at every production and diagnostic entry point, preserve
the immutable diagnostic reservation, exact final-source handoff, native
provenance and current durable writer limits. Finish the lost-response and
stable typed-hold/wake contracts above before adding new per-part state.

Owner #1709/#1952. The six findings and source comparison block implementation
handoff and activation. They are design work to resolve here, not questions
the owner must answer or permission to weaken evidence. No schema, API,
resource policy, cache, original command or native state changed in this
investigation. Fresh-reader, conversation/source and all mapped seam checks
are required on the replacement design before handoff.

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
