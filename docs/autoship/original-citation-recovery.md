# Original witness citation recovery (#2080)

Status: investigation and test-first draft. No fix, implementation handoff or
rollout is approved by this document. Root implementation agent owns every open
thread below; resolve them against actual callers before handoff. This is normal
engineering, excluded from ordinary autoship selection. #1703 remains the full
acceptance contract. #2058 is separate unfinished engineering, not superseded.

## 1. Verified incident and scope

The normal November 13 20:49:59 UTC tick selected ordinary #1830. Generation 1
ended at `acceptance_contract_invalid / await_acceptance_input_change`, with
12 historical model calls, no PR and zero confirmed autonomous merges. Two actual
original plans/results remain: unexpected failure followed by intended failure.
This does not establish a qualified behavioral witness or candidate GREEN.

Read-only inspection found exact original source quotes in
`crates/augmentagent-store/src/draft_context_receipts.rs` at immutable original
`b9e09d46864928a90c78ceb52841b1e0d5968bb9`. Both start at line 41. A 29-line quote
declares end 68 but actually ends at 69; a 21-line quote declares end 60 but ends
at 61. Their exact bytes occur at that start in the original Git object; trimming
is not the explanation. Do not publish private replies or owner data.

## 2. Decisions and guarantees

D1. Exact immutable original source and native session provenance remain required.
Correct source metadata never proves direct-production measurement by itself.
Every criterion, private oracle/raw observation, actual invoked production path,
original fixture and independent semantic judgment remains required. Candidate
GREEN, independent final review, hosted CI and exact-head merge remain later gates.

D2. Raw native output is immutable. Any checked canonical citation must be derived
separately, retaining its original raw-output hash, original declared metadata and
exact original Git source anchor. Never rewrite a reply/provenance file or pretend
a derived JSON value was the provider's response.

D3. New/retained normal current-policy validation may correct an unambiguous end
count only after verifying the complete exact quote at the declared original
path/start. No searching for a different start/path, fuzzy matching, whitespace
rewriting, candidate source or invented missing quote is allowed. Keep existing
1–16 citations/verifier, <=600 source lines/citation and <=512 KiB total bounds.
The admitted repair is a single inclusive-end error in either direction. Both
declared and derived ranges must independently stay inside the original source
and the 600-line bound; a larger discrepancy remains invalid. Derive the end
from `quote.split('\n').count()` at the declared start and compare the full quote
to the existing v1 `source.lines()` view joined by LF. Do not trim the quote,
strip CR from it, discard a terminal newline, or change source decoding. Rust's
existing line view removes source CRLF terminators; that is the established v1
protocol, not a new fuzzy-quote repair. Wrong/absent semantic judgments remain
negative. This envelope fixes the verified redundant endpoint count while
leaving contradictory or missing source evidence rejected.

D4. An old-policy reply is data, not current qualification. A parser-only fix is
insufficient. Preserve normal v1 artifacts and their native/request/policy checks.
Any explicit narrow requalification of exact retained metadata-error evidence
must be represented and independently reviewed as such; do not silently feed an
old context to a current artifact or synthesize new provider provenance.

D5. Preserve every native charge and existing finite workflow/input/environment
budget. No policy-only hash, new main commit, routing metadata, UTC rollover or
new generation may silently refill them. Metadata checking, free inspection and
queueing are not model calls. If fresh semantic adjudication really is necessary,
its legitimate existing-budget admission must be named separately; do not describe
it as a free replay. A valid decline/unknown is not an endpoint counting error.

D6. Existing source/owner/lease/pending-publication/inbox guards govern every
claim and mutation. No issue edit, forced resubmission, candidate rewrite, manual
SQL write, owner unpause, allowance reset or repair-PR self-test is authorized.

## 3. Current connected source map

- `self_improve_baseline_witness.rs::validate` decodes Judgment v1 and compares
  quote to `source.lines()[start-1..end].join("\n")`. It separately checks all
  criteria and semantic fields. Its source contributes to `policy_hash()` and
  hence the qualification context; changing it changes that current context.
- `qualify` first checks current artifacts, then authenticated retained Unknown,
  original checkout/environment and source recovery, then `persistent_witness`.
  `current_judgment` verifies native provenance under the current context before
  parsing; data-only retained readers explicitly do not import old approval.
- `self_improve_scope.rs::persistent_witness` retains original raw/proof pairs.
  Its completed-response protocol has TWO slots: original question, then exact
  `correction_prompt(original,error,previous_raw)`. Workflow binding contract and
  context participate in pair keys. Actual native admissions remain separate.
  Duplicate raw values can appear under multiple indexes; do not count rows as
  separate native calls. `witness_policy_hash` hashes this producer's source.
- `verify_witness_reply` binds original head, role, context and native producer
  policy. `retained_judgment_data` also reconstructs the complete original
  `qualification_prompt(context,prepared)` request hash, rather than trusting a
  self-reported context. Corrective requests need their exact original chain.
- `autoship_qualification_schedule.rs` currently selects infrastructure/evidence
  holds and exhausted/declined amendment cases; it omits `acceptance_contract_invalid`.
  `qualification_recovery::native_hold` likewise means only Scope evidence-unknown.
  `recover_semantic_qualification` and `inspect_semantic_hold` recover negative
  Unknown/legacy evidence through existing free inspection and atomic one-use cue.
  Merely adding the missing reason to a SQL list grants no valid recovery.
- `self_improve_contract.rs::retained_semantic_source` can locate prepared data
  from saved enrollment or original result records, but selects retained Unknown;
  a positive quote with invalid endpoint does not fit that semantic predicate.

## 4. Proposed data path, not yet approved implementation

One existing witness service should own the checked correction. Candidate shapes
include a derived citation record and a versioned qualification artifact retaining
both original native context/provenance and current checked context. A live grant
must require complete original native-request authentication, raw captured
measurement/original plan binding, exact source anchor, full judgment validation,
current caller authority and an explicit narrowly reviewed migration rule. A
serialized correction reference alone grants nothing. Do not introduce a second
approval authority/ledger or change the native producer policy merely to evade a
saved result's checks. Any producer change must be explicit and budget-safe.

The existing free inspector should recognize only that proven recoverable case,
reserve a bounded fair selection, revalidate authenticated issue/PR source and
owner request, atomically cue once, then let normal claim/preparation replay the
same retained evidence. Queue/event failure must roll back; source loss remains
held and can restore the same cue. Closed/paused/retired sources never dispatch.
The cue identity must bind actual immutable evidence and repair rule, not a new
build hash or retry count. Existing source retirement and manual-merge accounting
remain authoritative. Mutation/publication and data-only inspection stay separate.

## 5. Build order and testable exits

1. Reproduce both live range/quote shapes at the actual parser against a pinned
   temporary original checkout. This test establishes source-metadata failure,
   not provider judgment, original behavioral RED or workflow recovery.
2. Resolve section 6 with actual native request/pair, current artifact and
   initial/enrolled caller sources; write the concrete chosen representation,
   complete byte/hash identities, reader/writer/migration rules and ownership.
   Run fresh-reader and every-seam audit before calling it ready to implement.
3. TDD canonicalization and negative source cases, then authenticated retained
   result replay across policy contexts, both completed-response slots, ambiguity,
   lost/changed/exposed proof, missing raw command capture and interruption.
4. TDD the actual initial-unenrolled and enrolled free selection → cue → normal
   claim → preparation → qualification chains, including restart, duplicate,
   source/owner races, missing publication and unchanged failed retries. Verify
   literal before/after charges and zero extra calls for metadata-only replay.
5. Normal independent engineering review, exact-head hosted CI, supported deploy
   snapshot, protected merge and updater rollout. Observe naturally selected
   ordinary work end to end; this repair is never an autoship test case. No
   benchmark. Full #1703 fault/soak, capacity, CLI/private Discord and shipping
   acceptance still apply.

## 6. Open threads — all block implementation handoff and rollout

- Repair envelope resolved in D3: only +/-1 inclusive end, exact full quote at
  declared start, independently bounded declared/derived intervals and unchanged
  v1 source-line semantics. Canonical metadata/artifact representation is still
  open; root must specify original versus derived fields before step 2 completes.
- Positive old-context handling: determine how the current witness authority can
  requalify the exact complete original judgment with a metadata correction,
  while keeping original native provenance/context and the old-data-only
  invariant. If that cannot be proved, retain the hold and identify the legitimate
  existing-budget next action rather than synthesizing qualification. Root/step 2.
- Authenticate original versus correction-slot questions, actual native admission,
  original measurement and independent-session ownership. Do not infer roles or
  multiplicity from raw index counts. Root/step 2 and actual saved-data probe.
- Specify stable cue/rule identity, all guards, typed source-loss/unknown failures,
  bounded fair eligibility and both actual initial/enrolled consuming callers.
  Root/step 2; parser-only success cannot close the issue.
- Publication/recovery: define private artifact lifecycle, immutable raw reuse,
  versioned v1 compatibility, crash points and terminal data-only settlement
  versus new-work authority. Root/step 2; no second queue/ledger.

## 7. Non-goals, risks and terms

No automatic approval of a changed measurement, owner requirement or candidate;
no native/model switch, extra per-input slots or new workflow engine; no #2014
resumption. #2014 is closed/retired and must stay untouched. Source-metadata
fixture judgments are scripted test data, never actual provider acceptance.

"Canonical citation" is checked source data. "Qualification" is the existing
independent witness service's authenticated decision about a complete original
measurement. "Cue" is a one-use scheduling fact, never qualification or budget.
An old reply may be useful data even when it cannot grant current qualification.

Accepted risk during investigation: #1830 remains held and has no shipped fix.
Its finite completed-response attempts/charges and input hold contain repeated
spend. This risk ends only after normal deployed recovery/ordinary acceptance,
not a passing parser test or this draft's publication. No schema/receipt shortcut
is accepted as deferred implementation; all section 6 joins block activation.


## 8. Current test/source checkpoint (not implementation acceptance)

The parser regression failed for the intended source-metadata assertion after a
successful managed build (2 minutes 44 seconds; one failure, 10.07 seconds). It
reproduces the 41–68/actual-69 and 41–60/actual-61 shapes in a pinned temporary
source fixture. Its judgments/observations are scripted data; it is not original
production behavioral RED or an actual provider approval.

A stronger explicit read-only actual-#1830 probe passed (0.13 seconds after a
1 minute 30 second managed compile). It authenticated four retained native
observations, one complete captured reproduced original result, one exact
original witness question, one exact corrective-question chain and two exact
source endpoint errors. Snapshot and events stayed identical at twelve recorded
model calls. It created no artifact, workflow, qualification, cue or dispatch.
The complete corrective question was reconstructed from the unchanged two-slot
protocol, exact old validation error and full prior response; the actual case
is below that protocol's existing feedback preview limit. General oversized
feedback/protocol compatibility remains implementation work, not proved by this
one saved-data probe. Neither quote contains CR or a terminal LF.

The first narrower inventory passed but lacked raw-capture/corrective-chain
verification. Intermediate expanded builds failed on APIs available only on the
unmerged #2058 branch; those compile failures were not behavioral RED. The main-
based repair now exposes a small read-only original-result data reader over the
existing protected plan/admission/capture/quiescence records and stored proposal
hashes. It grants no execution, qualification or budget. Reader component checks
pass 26 tests with zero failures and two explicit delegated-execution ignores
(134.69 seconds). They include missing capture, missing/changed streams, cross-
plan results and an unbound transition even when environment IDs are equal.
The final historical-transition fixture is decoupled from live-producer behavior;
its focused negative test passes (51.66 seconds). Final integration validation
remains required before handoff/rollout.

Fresh-reader audit identified six acknowledged open boundaries, including old-
context authority, artifact lifecycle and cue consumption. Its report is retained
locally. A map lists 54 seams; that is a map, not 54 passing seam audits. No
implementation-readiness or engineering-review approval is claimed. Resolve the
remaining section 6 items and check every seam before handoff.


The current parser component selection passes 18 tests with zero failures and
two explicit ignores (49.27 seconds). It covers both observed too-short endpoints,
a one-line too-long endpoint, and rejection of wrong starts/quotes, CRLF in the
quote, a trailing LF, a larger discrepancy, out-of-source/before-start metadata
and candidate-only source. A second actual read-only probe after canonicalization
passes in 0.12 seconds: it keeps each original native context and twelve model
calls unchanged, and still emits no qualification or dispatch. These component
results do not implement section 4's positive requalification or wake-up path.

## 9. Candidate architecture to resolve before handoff

The following is a source-grounded candidate, not permission to implement around
the unresolved reader/caller contracts in section 6:

- Keep the native producer unchanged for this repair: its actual proof policy
  currently matches the saved observations. The qualification checker changes
  policy/context independently. A requalification predicate must authenticate
  the complete original question for the complete unchanged prepared measurement,
  using its original context, plus any exact corrective-question chain. A
  self-reported context, provider name or unverified saved JSON is insufficient.
- Let the existing witness service represent current qualification separately
  from the original independent native judgment. A possible Artifact v2 carries
  current checked context/policy, original native context/raw hash/provenance,
  exact original plan/result/capture identity and explicit declared/canonical
  source metadata under a fixed endpoint-repair rule. It never serializes a
  modified value as the original provider response. V1 reads remain strict.
- A metadata-positive requalification cannot use the negative-Unknown cue path
  as if those were the same decision. The existing recovery mechanism needs a
  typed distinction consumed by actual preparation before any new scoper call;
  negative reconsideration still routes to its existing independent repair.
  Define this distinction and all old/new cue readers before implementation.
- Stable cue identity should derive from immutable workflow/native/measurement/
  source evidence and the semantic repair rule, not current build hashes or
  routing metadata. Snapshot, generation and owner revision still fence the
  transaction. Do not erase or refill existing completed-response slots/charges.
- No positive requalification may override an authenticated completed negative,
  Unknown or decline for the applicable original/corrective question chain.
  Missing/ambiguous native ownership, captures, questions or source stays held.
  Do not arbitrarily pick a raw row when several indexes refer to one call.
- A reader must reauthenticate original data/provenance and current checks;
  a path, serialized correction reference or previous policy hash alone cannot
  grant current qualification. Map current `assert_current`/`current_data`,
  source-context consumers and workflow/store ownership before choosing stored
  database/control references or an artifact identity. Keep raw-data inspection
  distinct from live source/owner/lease authority and terminal retirement.
- Resolve publication crash recovery, typed unavailable causes and dependency-
  changed wake-up before activating the cue. Repeated failed writes on identical
  evidence must not create another cue/model attempt or an unchanged reclaim
  loop. Source restoration must restore the same episode, not invent a budget.

These points narrow the remaining investigation; they do not close section 6,
complete the 54-seam audit or establish a supported deployed recovery path.


## 10. Additional rollout seams found during source comparison

Both changed checker/contract sources contribute to the qualification policy
hash. Consequently this rollout also changes the context of previously valid
v1 qualifications, not only #1830's metadata-invalid replies. Resolve a supported
cutover for applicable existing positive receipts before rollout. A compatibility
path must prove the complete unchanged semantic question, original measurement,
current structural requirements and genuine independent native provenance; it
cannot import approval solely from an older policy hash. Changed/inapplicable
questions, negatives, Unknown and uncertain admissions stay held. Do not cause
new paid qualification merely because this patch or a later unrelated source
change produced another code hash. This broadens the compatibility investigation;
it does not authorize a blanket old-positive migration or new budgets.

Main reconciliation and enrollment can change the live binding's head/contract/
evidence hash. Immutable original facts retain their historical plan binding and
original base. Map when a cue is read/claimed, when main is integrated, when
acceptance is bound, and when prepared evidence is published. A legitimate head
rotation must neither lose the cue nor mint another one. Historical binding is
data identity, not authority over the current candidate. Source/owner/lease and
all exact-current-head candidate/review/CI checks remain separate.

`assert_current` and `current_data` currently lack a WorkflowGuard argument, while
the new fact reader requires protected original-store evidence. Define who
supplies and authenticates the original workflow/store/control references at
publication and on every reader/restart, without another authority or ad hoc
copies. Preserve terminal data-only audit versus permission for new work. Check
resource/native callback ordering so revalidation cannot fence its own legitimate
in-flight operation or bypass an unresolved earlier one. Source-context consumers
must distinguish original native context from current checked qualification
context; a relabeled JSON field cannot replace that distinction.

These are root-owned load-bearing rollout blockers under section 6. They are
implementation facts to resolve from the existing services, not requests for a
new owner product decision. The parser and read-only evidence checkpoint is
committed on an engineering branch; it is not deployed and has no repair-test PR.


## 11. Corrective negative-source authentication checkpoint

A connected regression found that retained diagnosis authenticated only the
original qualification question and silently missed a valid corrective Unknown.
The test failed for that intended missing-diagnosis assertion (5.08 seconds after
successful compilation). The data reader now authenticates the exact bounded
corrective question against its original complete request, previous response and
checker rejection. It supports the current checker and the explicit pre-citation
checker policy at deployed main b9e09d46; unknown historical corrective protocols
are not guessed. The frozen policy hash was reproduced from immutable Git source
and verified against both actual #1830 witness contexts.

Original-question identities remain unchanged. A corrective diagnosis instead
binds its actual authenticated corrective-request hash. The original response,
proof files, role and native producer policy remain unchanged; no extra model or
completed-response slot is created. This is negative/source data recovery, not
current positive qualification or permission to run work.

The witness components pass 19 tests, zero failures and two explicit ignores
(55.02 seconds). Adversarial coverage rejects a missing predecessor, changed
predecessor, altered request and ambiguous chains. A fixture compilation error
while adding those negatives was corrected and is not behavioral RED. The
production request matcher also passes the read-only actual-#1830 inventory in
0.15 seconds: one complete captured result, original/corrective questions and
both endpoint errors authenticate, with all twelve recorded calls and workflow
events unchanged. No qualification, cue or dispatch was created.

Recognizing both questions also means two compatible positive replies can exist
under one original native context. Distinguish source-context deduplication from
selection of a particular semantic decision. Existing conservative ambiguous-
identity behavior is not a completed positive-replay selection rule. Resolve that
consumer seam with negative precedence and the explicit current/native context
contract before activating the bridge. The versioned receipt, valid-positive
cutover, typed wake-up and actual preparation/binding integration remain open.


## 12. Controller-slot evidence for the next integration step

Read-only production inspection found both existing protected completed-response
slots for #1830: slot 0 and slot 1 each retain their raw value and native-proof
reference. Their keys are the existing `response_key("baseline_witness_v1",
origin_context, controller_binding.contract, false, slot)` and the associated
native-scope provenance key. One original native context covers both slots.

Prefer verifying these authoritative controller associations, then matching each
raw/proof pair to authenticated native custody, rather than making positive
replay depend only on reconstructing every historical validator's error text.
This is an implementation candidate, not a grant from a key or a JSON row. The
original slot's complete semantic question and original captured measurement must
still match. Every later slot must be considered; an applicable completed
negative/Unknown cannot be ignored. Missing, changed, unclassified or ambiguous
associations remain unavailable for positive replay.

The actual case's controller binding matches the saved original plan binding.
For other enrollment/main-reconciliation paths that is an unverified assumption:
map and authenticate the historical controller binding actually used for the
slots, and keep it distinct from the live binding that authorizes current work.
Do not guess that a current head/contract is the original indexing authority.
No controller slot, native record, model charge or workflow was edited to obtain
this evidence. The receipt/wake-up and compatibility work remains open.
