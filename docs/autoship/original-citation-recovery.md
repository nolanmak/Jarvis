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
Explicit requalification of exact retained metadata-error evidence and previously
exact positive evidence must be represented and independently reviewed as such.
Both require the complete unchanged native question, actual original measurement
and current checks; the latter avoids stranding valid work solely on rollout.
Do not silently feed an old context to a current artifact or synthesize new
provider provenance.

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
- `qualify` checks current artifacts, authenticated retained Unknown and the
  original checkout/environment, then explicit retained requalification (§15)
  before source recovery/`persistent_witness`. V1 native/current-context checks
  remain strict. V2 separately rechecks original captured/native/controller data
  and current validation; source/data readers alone never import old approval.
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

The existing witness service owns the checked correction. Section 15 chooses a
version 2 qualification artifact retaining original native context/provenance
separately from current checked context. Its live writer requires complete
original native-request authentication, raw captured measurement/original plan
binding, exact source anchor, full judgment validation, current caller authority
and the explicit reviewed migration rule. A serialized correction reference alone
grants nothing. The implementation remains on the engineering branch; held-source
cue activation and review are unfinished. Do not introduce a second
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
  v1 source-line semantics. Section 15 now specifies/implements the version 2
  representation on the engineering branch. Root still owns its complete reader,
  cue/consumer integration and audit before step 2/handoff is complete.
- Positive old-context handling: section 15 adds explicit current validation and
  a separate version 2 receipt in the existing witness authority. Original native
  bytes/context remain unchanged. Complete the held-source cue/preparation and
  all caller/restart audits before activation; unsupported/missing evidence stays
  held without a fresh metadata-only model call. Root/step 2.
- Section 14 now authenticates original/correction slots via actual charged
  bindings, full questions and original captures; the actual saved-data probe
  passes. Preserve independent native role/ownership and negative precedence
  through the version 2 receipt and every consuming caller. Root/step 2/audit.
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
  from the original independent native judgment. Section 15's Artifact v2
  carries current checked context, original native context/raw hash and checked
  canonical judgment digest, plus original plan/result/capture/native identity
  under its fixed rule. Declared metadata remains in immutable native raw data;
  canonical metadata is recomputed and digest-checked on every read. It never
  serializes a modified value as the provider response. V1 reads remain strict.
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
  grant current qualification. Section 15 chooses the authoritative references
  for `assert_current`; finish the `current_data`/source-context/consumer and
  callback audits. Keep raw-data inspection distinct from live source/owner/
  lease authority and terminal retirement.
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

`assert_current` and `current_data` lack a WorkflowGuard argument, while the
fact reader requires protected original-store evidence. Section 15 now defines
version 2 publication and restart references: actual guarded store, exact
configured database path, source/workflow identity and existing native control
namespace, with consistent read snapshots. Finish the caller audit without
introducing another authority or ad hoc database copies. Preserve terminal data-only audit versus permission for new work. Check
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
under one original native context. Source-context deduplication now uses the set
of fully authenticated contexts, while semantic decision consumers preserve
unique-reply identity (section 13). This resolves the source-only consumer seam;
it does not supply positive-replay selection. Resolve that remaining decision
seam with negative precedence and the explicit current/native context contract
before activating the bridge. The versioned receipt, valid-positive cutover,
typed wake-up and actual preparation/binding integration remain open.


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

The actual case's controller binding matches the saved original plan binding,
but neither that equality nor the live binding is now used to identify slots.
Each original paid native admission already records its exact bounded binding.
Section 14's reader returns that binding with the original completed native data
through the existing custody validator. Both actual #1830 witness admissions
match their controller slots through those original admission bindings. That
resolves the historical indexing input without assuming that enrollment or main
reconciliation preserved the original plan binding. The historical binding is
data identity, never current execution authority. No production controller slot,
native record, model charge or workflow was edited to obtain this evidence.
The receipt/wake-up and compatibility work remains open.


## 13. Source-context identity regression and saved-case proof

The source reader previously required a unique reply identity even when every
fully authenticated reply described the same original source context. Its new
regression failed for exactly `retained semantic judgment is ambiguous` after
successful compilation (5.12 seconds). Authentication is now shared separately
from consumer uniqueness. Source-only projection deduplicates contexts and keeps
the existing preference for the current authenticated context; decision and
negative-diagnosis consumers still require a unique applicable reply identity.
Different historical contexts remain ambiguous, and changed native bytes fail
custody validation. Source lookup does not persist or import qualification.

Final witness components: 20 passed, zero failures, two explicit ignored probes
(54.76 seconds). The expanded actual-#1830 read-only probe also passed (0.24
seconds): both actual replies resolve to their original shared context, while
unqualified positive decision selection remains ambiguous. One complete captured
original result, both exact native question roles, both endpoint errors and all
12 recorded model calls remain present; snapshots and events are unchanged.
These are saved-evidence/source-projection checks, not an autonomous recovery or
merge. The live workflow remains generation 1 at `acceptance_contract_invalid` /
`await_acceptance_input_change`, and live health reports no confirmed autonomous
merge since November 6. No model dispatch or workflow mutation was requested.

This change still affects the qualification policy hash because its source is
hashed. Do not deploy this branch before completing the versioned receipt,
valid-positive compatibility, typed cue, real preparation/binding integration
and the audits required by sections 5, 6 and 10. The controller-slot evidence in
section 12 is the next integration input, not an implemented grant.


## 14. Charged-admission binding and composed controller-data checkpoint

`VerifierEvidenceInspection::native_admissions` now exposes each completed native
observation together with its original charged binding. It shares the existing
paid-custody validator with leased execution: provider/reviewer attribution,
single admission, charge, bounded counter and completed raw/proof custody remain
mandatory. The binding is not recovered from today's checkpoint or guessed from
a verifier plan. A read-only restart/rotation test passes and rejects loss of the
original charge. Existing native-custody regressions also pass (four tests,
0.28 seconds), including unanswered outcomes, binding changes and lost custody.
The native producer source/policy is unchanged.

`retained_controller_slots` uses the recorded admission contract to derive the
unchanged two controller keys, then matches each raw/proof pair to its actual
native admission. Both slots remain in order with their original raw bytes,
proof references and admitted bindings. Half-pairs, absent admitted responses,
foreign proof, duplicate ownership, changed native proof and a different
corrective request remain errors. The complete original semantic question must
match the provided prepared measurement. Corrective question authentication
still supports only the explicit current/pre-citation checker protocols from
section 11; unsupported historical protocols remain unavailable. Controller keys
alone do not replace native question/provenance authentication.

`retained_original_witness_data` composes that reader with
`exact_original_result`: every call rechecks protected plan/result records,
admissions, quiescence, capture manifests/streams and receipt digests before
returning the original measured data and ordered native/controller observations.
Its stable data identity binds the actual original-result identity and all
controller-run identities, excluding current build, generation and live head.
It returns no execution capability, qualification artifact, scheduling cue or
model admission. The production authority/cue integration must use this composed
path within its supported inspection/lease transaction; a bare source-only or
controller-only result cannot qualify the measurement. Publication/restart
readers still need the explicit authoritative store/control reference and
current/native context contract from section 10.

Final witness components: 21 passed, zero failures, two explicit ignored probes
(57.86 seconds). New disposable fixtures preserve a corrective Unknown and its
negative disposition across a live binding change; they reject missing controller
output, a foreign controller proof, lost native proof and a changed corrective
question, leaving fixture events/charges unchanged. Their native replies and
command claims are scripted fixtures, not autonomous production acceptance.
The first run rejected an invalid fixture proof filename before the intended
assertions; the fixture was corrected, and that failure is not claimed as a
behavioral RED for this feature.

The actual #1830 composed reader passes read-only (0.36 seconds): one complete
captured original result and both exact-question controller slots authenticate
together via their original paid admission bindings. Four native observations,
two exact endpoint errors, all twelve recorded model calls, snapshot and events
remain unchanged. No qualification, wake-up or dispatch was produced. The
versioned receipt, valid-positive cutover, decision/negative precedence, typed
cue, actual preparation/binding integration and full audits remain required
before this branch can be deployed. These data checks do not restore autoship.


## 15. Explicit current requalification receipt and caller regression

The actual `qualify` regression first authenticates a complete disposable command
capture and original native/controller response under the pre-citation context.
It then requires the real caller to requalify without fresh native generation.
The intended RED reached `fresh native scoper unavailable` (20.62 seconds after a
successful managed compile); its test commit is `83467163`. An earlier run stopped
at an invalid fixture storage location under the worker-writable build cache.
Moving that disposable state to private `/tmp` fixed the prerequisite; that
storage rejection is not the qualification RED.

Chosen rule: `retained-original-witness-current-validation-v1`, implemented by
this same baseline witness service. It rechecks exact protected original facts,
all applicable ordered controller/native replies and the complete unchanged
qualification question. Every structurally valid semantic reply must have direct
production measurement and its original fixture. An applicable negative/Unknown
blocks qualification, including an earlier one; no convenient positive run is
selected. A structurally rejected first reply can remain diagnostic input only
when its exact authenticated corrective response exists and passes current
checks. A final malformed reply is never skipped. Unsupported correction
protocols, missing original records/captures, ambiguous originals or changed
physical evidence produce a hold rather than metadata-only paid regeneration.
A genuinely fresh original without any saved witness continues the normal native
path; free lookup does not create a missing control directory.

Version 1 artifacts keep their strict existing native/current-context validator.
Version 2 is an explicit separate shape under the existing protected witness
artifact path. Its fields bind current context and fixed rule; repository,
workflow, authoritative workflow database and native-control directory; exact
original plan/result references and composed immutable-data identity; and each
checked reply's ORIGINAL native context, slot/input, raw-response SHA-256 and
canonical-judgment SHA-256. Current validation canonicalizes only D3's endpoint
metadata. The native reply/proof and its declared citation remain immutable;
checked metadata is a derived value whose digest is rechecked. No native context
is relabeled as current. The existing 64 KiB private artifact envelope applies;
oversize is a hold, never truncation or another paid reply.

Writer and readers use the existing workflow evidence service, not another
approval ledger. Publication runs inside the actual guarded store's immediate
transaction: lease/binding, supported owner/owner-turn dispatch, active stage and
unsettled-publication guards precede derivation. It authenticates the prepared
source, verifies original checkout/integrity/environment and requires the actual
store path to equal configured workflow authority. All artifact, native-control
and store locations must lie outside worker-writable roots. Publication uses the
existing immutable private file protocol. If publication/commit fails, the caller
fails; any complete file left by an interrupted publication can only be used
after full restart revalidation and the caller's normal live dispatch guards.
No charge, workflow transition, approval event or new execution authority is
created by that file alone.

A restart reader requires that same configured private database, canonical
repository and matching source/workflow identity (including supported PR alias),
plus the existing repository/workflow-derived native namespace. It opens a
read-only SQLite snapshot and reauthenticates original plan/result, actual
capture streams/receipts, every original native/controller association, composed
data identity and all current checked digests. The caller's current registration
integrity check remains. This allows terminal data audit but grants no permission
to claim or dispatch a terminal/paused workflow. Candidate GREEN, independent
review, exact-head CI and merge remain separate gates.

Current focused caller GREEN passes (20.68 seconds). Expanded witness components
pass 23 tests, zero failures, two explicit ignored probes (58.87 seconds), including
repeat receipt idempotence, distinct current/native contexts, fresh/no-native
behavior, restart rejection of changed store/workflow/rule/checked digest,
changed original command output and changed native provenance. Restoring the
same actual evidence restores the same receipt's validity with unchanged charges.
A separate seven-scenario publication-fence fixture passes (0.24 seconds): normal
read succeeds, while pause, pending publication, retired/held stage, expired lease
and wrong binding refuse derivation before its callback and preserve events.
All replies/command facts in those unit fixtures are scripted, not production
behavioral or autonomous-shipping acceptance.

The actual #1830 read-only probe also passes (0.37 seconds): both saved replies
satisfy the proposed current source/semantic validation against the complete
captured original result. It writes no receipt, grants no live qualification,
creates no cue and dispatches no work; all twelve calls and workflow events stay
unchanged. Before the subsequent freshness hardening, full CLI validation passed
2,071 tests, zero failures and 42 explicit ignores (340.84 seconds). The final
post-hardening full CLI passes at the section 16 checkpoint. The positive
held-source typed cue and actual initial/enrolled preparation/binding/main-
rotation consumption remain UNIMPLEMENTED, with all corresponding source-loss,
fairness/crash/budget tests and audits still required. Do not deploy this branch
or claim autoship restored from the caller/component checks.


## 16. Fresh audit triage and authoritative freshness

The fresh-reader audit returned seven findings. Root checked each against the
current source/spec. The positive cue payload/consumers, complete cue lifecycle,
binding/head-rotation rules and scheduler bound/fairness remain real open work
(findings 1–3 and 7). Root owns them as the next implementation stage; they block
activation and handoff, not owner product decisions. The previous 54-seam map is
outdated and still is not a completed seam audit. Refresh/check all seams after
the remaining integration is concrete.

Finding 4 exposed a real freshness defect: removing native-admission events AND
bounded counters while leaving paid charges/controller proofs caused the reader
to return empty, allowing it to appear fresh. The targeted regression failed for
that intended assertion (30.86 seconds). Native lookup now reconciles original
paid custody with the controller's committed provenance-key associations before
returning any runs. A provenance key is exactly the versioned hash of another
known raw slot key (§17); a reference-shaped model reply is not a provenance
record. Such raw text remains data and cannot claim native history. Its separate
regression failed for that classification error (5.23 seconds) before the
association check was added. This retains an actual controller boundary rather
than trusting JSON shape, a filename supplied by a model, or raw row counts.

Existing custody errors distinguish incomplete/unanswered admission, missing
committed response, invalid native provenance and unsupported corrective history;
none permits a fresh metadata-only call. A tagged provenance reference absent
from paid custody raises the existing unresolved-native-outcome error. If native
custody is empty but the protected native directory retains a native proof or
witness artifact marker, history is unavailable, not never-admitted. This also
catches an otherwise unindexed protected history marker. A genuinely absent
native directory with no admitted/committed history needs no recovery directory
and is not created by lookup.

Freshness applies to the COMPLETE unchanged witness question: all global known
native custody must be coherent, and there must be no applicable saved witness
for that question. A coherent prior source-selection call or witness for a
DIFFERENT prepared measurement does not qualify the current measurement; its
normal next question uses the existing finite budget. A saved applicable witness
without matching captured original records is held, not regenerated. Current
policy/context hashing alone cannot make an unchanged question fresh: old
questions are authenticated using their original context and the complete current
prepared measurement. No paid charge/counter is erased or refilled by inspection.

Finding 5 is resolved by the versioned encodings and fixed controller/run vector
in section 17; future codec changes require an explicit new version rather than
silently changing a stable recovery identity. Finding 6 is resolved for receipt
files by the immutable-publication outcomes below. The remaining scheduling/
source-restoration transitions remain part of findings 1–3 and 7 and must still
be implemented/tested before deployment.

The final witness components pass 25 tests, zero failures, two explicit ignored
probes (59.68 seconds). They include both corruption and raw-data classification
regressions, the frozen wire vector and the restart/caller checks in section 15.
The final actual #1830 read-only probe passes (0.40 seconds), with the same twelve
calls and unchanged snapshot/events; no receipt/cue/workflow mutation is made.
The final full CLI passes 2,073 tests, zero failures and 42 explicit ignores
(342.12 seconds). Neither this checkpoint nor the fresh audit is engineering
approval or evidence of a live autonomous recovery.

## 17. Frozen identities and receipt publication outcomes

All new identity encodings below are compact UTF-8 JSON from
`serde_json::to_vec`, followed by SHA-256 and lowercase hexadecimal. Tuples encode
as ordered arrays; there is no added newline. Maps are Rust `BTreeMap` order;
struct fields use declaration order. Native raw response and recorded plan/result/
admission/quiescence hashes use their ORIGINAL stored bytes, never reserialized
replacement bytes. Path references serialize their exact canonical UTF-8 string.
Do not change a v1 field order/encoding under the same tag.

- Controller raw key: `["baseline_witness_v1", native_context,
  admitted_binding.contract, slot]`, slots 0 then 1.
- Controller proof key: `["native-scope-response-provenance-v1", raw_key]`.
  This tag belongs to the existing native scoping/witness producer; ordinary raw
  model output has no authority to choose a key in this protocol.
- Run identity: `["retained-controller-witness-data-v1", workflow,
  native_context, admitted_contract, replies]`. Replies are ordered tuples
  `[slot, native_input, admitted_binding, SHA256(original_response_bytes),
  native_reference]`. Binding fields are `head`, `contract`, `evidence_hash`;
  reference fields are `artifact`, `artifact_sha256`. ALL slots participate,
  including a structurally rejected diagnostic first reply.
- Exact original-data identity: `["exact-original-result-data-v1", workflow,
  plan_key, result_key, SHA256(original_plan_body), SHA256(original_result_body),
  captures, commands]`. `captures` maps verifier ID to `[command_key,
  SHA256(original_admission_body), SHA256(original_quiescence_body),
  capture_manifest_sha256]`. `commands` maps verifier ID to fields `exit_code`
  (integer or null), `stdout`, `stderr`, `interrupted`, `output_complete` in that
  order. Capture reading independently verifies the physical stream/manifest
  hashes and receipts; this hash is not a substitute for those checks.
- Composed witness-data identity: `["retained-original-witness-data-v1",
  exact_original_data_identity, run_identities]`. Run order is the reader's sorted
  `(native_context, admitted_contract)` grouping. It excludes current policy/
  build/generation/live head; ORIGINAL admitted head/context remain immutable facts.
- A checked-response digest is the compact serialization of current `Judgment`
  data: fields `version`, `context`, `verifiers`; each verifier has
  `verifier_id`, `criterion_ids`, `measurement`, `original_fixture_available`,
  `concrete_input`, `reason`, `production_citations`; each citation has `path`,
  `start_line`, `end_line`, `quote`. `context` remains the original native context;
  only D3 permits derived end metadata. Enum spellings stay v1 snake_case.

The receipt schema is exactly `RequalifiedArtifact` (§15): fields `version`,
`context`, `rule`, `repository`, `workflow`, `workflow_db`, `native_control`,
`plan`, `result`, `data`, `checked`. Checked record fields are `native_context`,
`slot`, `input`, `response_sha256`, `canonical_judgment_sha256`. It stores every
validated semantic reply in run/slot order; a rejected diagnostic first reply is
omitted from `checked` but remains bound by `data`. All unknown/duplicate typed
fields are rejected by the existing typed decoder. Writer/readers are this one
service, never independent approval issuers.

`controller_data_identity_matches_the_frozen_v1_wire_vector` supplies an
independent fixed vector. For context `c` repeated 64 times, admitted contract
`original-contract`, slot 0, raw key is
`42d6eb10975672c7b7b43801c64d5aa42f27a1e41c4bc8eeb37bac797495d2fe`
and proof key is
`1bfd3c23c19e1d5297d4fdb3df5145066cabf4a101d1b137eceef9f5c03c2796`.
Its one-reply run vector (literal binding/reference/input/response bytes are in
the test) yields
`1c14cf1ff60dbf28289fc7ce7598289e7ddd543941a9b087bdcadfdd07653f7f`.
These are synthetic wire values, never provider evidence or a qualification.

Receipt publication follows the existing private immutable protocol:

| Interruption/state | Result and ownership |
| --- | --- |
| Before final publication | Named private temporary file is not an artifact. Normal error/drop cleanup removes it; unclean-exit leftovers remain under existing protected-state lifecycle/retention ownership (#1799), never build-cache pruning. A receipt temporary file is bounded by 64 KiB. No current grant or model retry follows. |
| Complete final file, caller/commit interrupted | Restart may use it only after full current/original/native revalidation and normal live dispatch guards; data alone does not create a lease or refill budgets. |
| Existing byte-identical final file | Idempotent reuse; no rewrite or new publication episode. |
| Partial, conflicting, malformed or nonprivate final file | Current validation fails and work remains held; never overwrite a native reply/proof or spend a model call to replace it. A conflict is not proof of a negative semantic decision. |
| Original evidence lost, then same bytes restored | Full restart revalidation restores the same receipt's validity and charges; the unit caller test exercises this. |
| Changed current policy with complete compatible original facts | A separately checked current-context receipt is derived by §15. Old receipts/raw facts remain immutable; no new model allowance follows solely from the policy hash. |
| Inapplicable/negative/unsupported native history | Hold or existing independent negative-repair path, as applicable; no current positive receipt is manufactured. |

A final conflict/tamper hold needs restoration of authentic controller state or
an explicitly supported repair of derived metadata; this change does not add an
operator overwrite command. No raw database copies, ad hoc rollback directories,
receipt deletion, snapshot-limit override or native-response rewrite is an
activation path. Existing protected-state retention owns obsolete evidence;
this patch adds no second copy or independent retention pool. The typed cue must
retain its same episode through failed publication and genuine source restoration
without unchanged productive reclaims; that scheduling work is still open.


## 18. Retained infrastructure continuation implementation checkpoint

The repair branch now preserves the precise
`acceptance_verifier_infrastructure_unavailable /
await_verifier_infrastructure_recovery` hold together with its authenticated
retained-original-witness reference. Checkpoint and continuation commit in the
same lease transaction. Repeated identical reports are idempotent. This does
not convert an older negative semantic judgment or an arbitrary infrastructure
hold into retained positive-source recovery.

Free selection can reserve this retained continuation for inspection, including
when original custody still needs restoration. Selection is not validation:
normal execution remains parked until the retained source validator succeeds
with the same reference under the existing owner, binding, effect and lease
fences. Restoration reuses the original cue and records the particular loss
episode, without another original-witness recovery grant, model call, token
charge or amendment allowance. Enrolled qualification errors use this path while
keeping their actual infrastructure reason and private diagnostic.

The test-first store regression failed because the infrastructure checkpoint
lost its retained continuation. The corrected path covers settlement followed
by failure, duplicate reporting, unchanged-input refusal, free reservation,
validated restoration and resettlement. Negative tests cover legacy semantic
references, generic infrastructure holds, foreign reference/binding, lost lease
and failure to persist the continuation. The real qualification-error fixture
also checks the public infrastructure reason and retained waiting reference.

This is an implementation checkpoint, not activation or ordinary end-to-end
acceptance. Production positive-source recovery still needs the original
read-only environment check, original initial-issue scope replay check and
publication/setup readiness gating connected before it can queue work. Do not
wire the data-only validator directly into the poller: valid original evidence
alone does not prove a failed publication or environment has recovered. The
read-only pinned-environment reader is present and tested but is not yet a
production wake condition. Current candidate environment, native custody,
original measurement, independent qualification and later merge gates remain
required. Final joined validation, documentation/seam review, independent
engineering review, hosted CI, protected merge and normal updater rollout remain
pending. No autonomous shipment is established by these component fixtures.
