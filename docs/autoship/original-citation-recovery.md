# Original witness citation recovery (#2080)

Status: implementation branch; final joined validation, independent review and
rollout remain pending. Root implementation agent owns every open thread below;
resolve them against actual callers before handoff. This is normal
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

## 6. Historical open-thread inventory — current resolution in §21

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


## 10. Historical rollout seam inventory — current resolution in §21

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
| Before final publication | Named private temporary file is not an artifact. Normal error/drop cleanup removes it; unclean-exit leftovers remain preserved protected state, never build-cache pruning. General crash-leftover retention is unfinished work owned by the root implementer under #1703, not by decomposition issue #1799. A receipt temporary file is bounded by 64 KiB. No current grant or model retry follows. |
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
activation path. This patch does not implement general obsolete-receipt or
crash-leftover collection. Root owns that remaining #1703 lifecycle work: define
protected references, interruption-safe collection and accounting before enabling
any sweep. Trigger review when a workflow terminates or receipt policy changes;
until then preserve evidence, account for its disk use, and hold on storage
pressure rather than delete evidence or override snapshot limits. This patch
adds no second copy or independent retention pool. The typed cue must
retain its same episode through failed publication and genuine source restoration
without unchanged productive reclaims; §§18–20 implement that scheduling and
readiness path on the repair branch. Joined validation and rollout remain open.


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


## 19. Read-only environment and initial-scope readiness checkpoint

The retained-source recovery inspector now takes the expected snapshot and its
protected execution context. It rejects a different workflow or binding, loads
and authenticates the complete original prepared evidence, then validates these
prerequisites before returning the reference to the store's recovery callback:

- Reconstruct the original environment from the immutable original Git base
  and original proposal overlays, and require equality with the registered
  environment. The read-only reader runs trusted Git object reads only; it
  performs no checkout, compiler/verifier invocation, model call or publication.
  Failure retains the infrastructure-unavailable classification.
- For an initial issue with no attached PR, load the original committed scope
  snapshot using the exact workflow, protected original frontend request,
  original base and registered environment. Missing, malformed or mismatched
  scope remains an evidence hold. Free readiness and leased execution use the
  same committed-response reader: a retained body without its matching
  `scope_stage_response_recorded` event is unavailable, not ready work. Preserve
  the original decision, complexity,
  criteria and fallback status. An attached PR uses its existing enrolled resume
  path and does not invent an initial issue scope requirement.

The shared initial-scope identity remains byte-compatible with the existing
`initial-scope-inputs-v1` tuple. The reader's bounds are 100,000 original tree
entries, 32 MiB of Git command output and 16 MiB per relevant compiler control;
these are inspection refusal bounds, not permission to execute candidate code.
Local Git replacement objects remain disabled. Candidate compiler edits do not
replace the immutable original inputs. Normal qualification still independently
checks actual original checkout, integrity and environment before granting any
builder authority.

The test-first regression reproduced an unavailable original environment being
accepted by the old data-only readiness boundary. Both initial-issue and PR
fixtures now exercise the shared recovery inspector through the store callback,
normal claims, preparation and same-reference restoration. Direct negative
checks cover an incompatible registered environment, a different protected
request and corrupt original issue scope; the attached-PR case remains valid
without an invented initial snapshot. Fault injection is limited to the
explicitly verified disposable fixture database. The actual ordinary-source
probe invokes this same inspector read-only and verifies unchanged live state
and accounting; it never queues or claims the source.

At this checkpoint publication/setup readiness remained required before the
production poller could invoke this positive-source path; §20 supplies that
boundary and connects the caller on the repair branch. The inspector's new prerequisites do not by
themselves prove that an immutable receipt can be published after an earlier
filesystem failure. The callback was still unconnected at this checkpoint; §20 supersedes that
implementation status. Production deployment remains unverified. No rollout, autonomous merge, fresh allowance or ordinary end-to-end
success is claimed here. Final joined checks, seam review, independent review,
hosted CI and the normal protected rollout remain pending.


## 20. Initial publication readiness and poller integration — refined in §22

D10. Separate read-only source validation from filesystem readiness. Source,
native, environment and original-scope inspection remains read-only (§19),
including the explicit ordinary-source inventory probe. A separate controller
readiness operation may allocate bounded anonymous scratch solely to exercise
actual allocation, quota, write and fsync. This is necessary because mode/free
space checks cannot prove that the next immutable publication can succeed.
It publishes no receipt, native response, manifest, qualification or grant and
runs no worker, verifier, compiler or model. It is normal recovery preparation,
not a substitute for original-checkout qualification or later merge gates.

The operation uses the same exact encoded v2 receipt bytes as the real writer.
A present final receipt must be private, bounded, single-linked and byte-identical;
conflicts, malformed/nonprivate files and symlinks remain held without overwrite.
The destination and configured authority must remain outside worker-writable
roots. The controller directory must be private, owned and writable. The probe
writes zeros equal to the bounded receipt + prepared data + manifest footprint,
plus 64 KiB of margin, and fsyncs both scratch and directory. Its maximum is
4 MiB + 192 KiB per operation; the existing four-candidate window bounds aggregate
scratch below 17 MiB. No budget or free-space-floor override is used.

On this Linux deployment the scratch descriptor is opened with O_TMPFILE and
O_CLOEXEC. There is no named-file fallback: an unsupported filesystem/platform
stays held. Thus failure, cancellation or process death leaves no named scratch
artifact or new retention pool. The descriptor closes on ordinary error/drop;
no snapshot, native proof or old receipt is deleted or rewritten. This probe
establishes current filesystem availability, not a reservation: an intervening
failure can still occur, and must return to the same restrictive hold.

The poller retains its existing fresh remote authorization/request checks,
RunLock, checkout validation, owner revision, binding, pending effects and inbox
fences. After existing authenticated negative-source handling, the positive
path reads any protected retained reference, runs source/input/publication
readiness, then uses the existing store transaction to queue the same workflow.
No reference or failed readiness means no productive claim or model admission.
A restored continuation reuses the same cue; no policy/build/time change refills
a bounded model or amendment allowance. Existing physical fallback is preserved.

Retained preparation now catches setup/publication errors at the real producer
boundary and records its existing source continuation before the public error
escapes. Enrolled evidence and infrastructure errors likewise retain that
continuation, preserving the actual safe reason and private diagnostics. Fresh
work without a retained reference keeps its existing error path. The wrapper
boxes the existing async preparation future to preserve the CLI's layout-depth
limit; it changes no invocation, timeout, provider or model budget.

Test-first publication checks reproduce a conflicting receipt being incorrectly
considered ready. Focused tests cover unwritable destinations, conflicting and
nonprivate receipts, dangling symlinks, bounded allocation and injected ENOSPC,
with unchanged receipt bytes and no leftover scratch. Both original issue and
PR lifecycle fixtures run the same readiness callback before queue and after
source restoration; a conflicting real-context receipt refuses queueing without
checkpoint/event changes. The explicit live #1830 inventory continues to use
only the read-only inspector. No live readiness probe or automatic wake is
performed by that inventory test.

This section supersedes the unconnected-publication status in §§18–19. Poller
integration is code on the repair branch, not deployed acceptance. Final joined
validation, fresh spec/seam review, independent engineering review, hosted CI,
supported snapshot, protected merge and normal updater rollout remain required.
Natural ordinary execution and the full #1703 outcome are still unproven.


## 21. Current recovery contract and unresolved integration blockers

This section is the current status authority. Earlier “UNIMPLEMENTED”, “next”
and open-thread statements in §§4–19 describe their individual checkpoints;
they do not override the contracts below. Code and component fixtures remain
engineering evidence, not deployed or ordinary-work acceptance. Root owns all
remaining implementation and rollout work; no new owner product decision is
required to resolve these seams.

### Cue identity and storage

The authoritative positive cue is a `workflow_event` with kind
`original_semantic_witness_recovered` and a strict `SemanticReconsideration` JSON
payload: `version:2`, `key`, `judgment`, `policy`, `original_generation`, `binding`,
`qualification_required:true`, and `source`. The source has tag
`kind:"retained_original_witness"`, an `evidence` object with fields `plan`, `result`, `data`, `prepared`, `contract`,
and a sibling `owner_request_sha256`.
Each evidence field, policy, judgment and key is lowercase 64-digit SHA-256;
`judgment` must equal `evidence.data`. The owner hash covers the exact protected
frontend request UTF-8 bytes. No relabeled provider response establishes this
source: the original plan/result, full raw measurement and paid native question
chain are independently authenticated before creation and on consumption.

`policy = SHA256(UTF8("retained-original-witness-current-validation-v1"))`.
The stable key is SHA-256 of the compact `serde_json` UTF-8 encoding of the tuple
`("retained-qualification-recovery-v1", workflow, evidence, policy)`. Evidence
object field order for this key input is the declaration order above. Stored
event JSON object order is not an identity rule: readers deserialize the typed
reference before reconstructing that key input. Live head/binding, generation,
owner revision, timestamps and current build hash fence transactions but do not
enter this key. The event's generation must equal `original_generation` and not
exceed the consuming generation. Current-policy qualification receipts may have
new context hashes; this does not mint a second cue or refill allowances.

The store uses an IMMEDIATE transaction and checks for the same workflow/event
key before inserting the initial cue. This is the deduplication authority;
there is no parallel queue or grant ledger. A v1 cue without `source` remains a
negative semantic reconsideration with its existing exact-binding rules. It is
never upgraded to this positive type or admitted as a retained infrastructure
continuation. Unsupported versions and mismatched references fail closed.

### Bindings and consumers

The original execution binding remains part of immutable plan/result evidence.
It is not overwritten with a later candidate head. Queueing compares the complete
expected snapshot, generation, branch and PR, eligibility and idle lease against
current store state. Protected owner revision/request, pause, pending mutation
and owner-inbox checks still fence this transaction. A stale observation queues
nothing. Remote source authorization, lifecycle/request equality, original
checkout ownership and RunLock are separately checked by the poller.

A live binding rotation is supported only by a protected current
`RecoveryContext` for that same workflow and binding. Its exact frontend request
hash must match the cue, and its contract must be that frontend request or
`registered:<original evidence.contract>`. Guard acquisition records this context
after audit/head rotation and before resolving the cue or installing accounting.
The immutable cue keeps its original binding; the current context supplies the
live association. A changed owner request or different registered measurement
cannot borrow this reference. A newer registered verifier manifest takes
precedence over historical recovery data, including when its evidence is lost.

For an initial issue the normal claim consumes the cue before paid lane,
readiness or scoping admission. The original committed scope is replayed using
the protected request, original base and original environment. It preserves
complexity, estimate, implementation guidance, criteria and fallback status.
A missing/corrupt scope stays held; it is never defaulted to a new hard grade.
For PR/enrolled resumption the normal source/PR identity fence remains and the
original prepared evidence is consumed without manufacturing an initial issue
scope. Both paths start with RecoveryOnly acceptance authority, use the actual
preparation/qualification writer, and must prove complete published prepared
bytes and current original qualification before settlement grants Qualified
builder authority. Exact-current-head candidate tests, review and hosted CI
remain later gates. Settlement is lease/binding/native-owner-turn fenced and
refuses unsettled publication or a waiting source.

### Episode transitions

| Current state | Event/condition | Result and durable authority |
| --- | --- | --- |
| Eligible metadata/native hold, no consumed matching key | Authenticated source and all applicable readiness checks pass under current snapshot/owner fences | One initial cue + Reconcile checkpoint commit atomically. No productive lease, model call or allowance is created. |
| Queued cue | Normal guarded claim | The normal lease/attempt rules apply; RecoveryOnly authority permits preparation, not a model bypass or merge. |
| Claimed/preparing or settled history | Required source/setup/qualification fails | Scope hold + same-cue `semantic_source_unavailable` version-1 record commit in the existing lease transaction. Its payload contains `cue` and the complete restrictive checkpoint. Evidence and infrastructure reasons remain distinct. |
| Same waiting checkpoint | Identical failure report | No new loss episode, grant or event. A generic retry cannot overwrite the hold. |
| Waiting source | Unavailable/mismatched source or readiness | Remains parked. Free inspection cannot create a productive lease. See remaining coverage blockers below. |
| Waiting source | Same reference passes source and readiness validation | Reconcile checkpoint + `original_semantic_source_restored` referencing the exact loss-event sequence commit atomically; no second initial cue is inserted. |
| Active cue after restore | Published prepared evidence equals the immutable reference and actual current qualification passes | `retained_qualification_prepared` records the same cue/data/prepared identity under the normal lease; builder authority becomes Qualified only after this succeeds. |
| Settled history, no later loss | Duplicate inspection/preparation or a later unrelated hold | The consumed stable key grants no second metadata wake or model budget. Historical evidence remains available for audit and provenance. |
| Settled history | A genuinely new recorded loss after settlement | That loss sequence reactivates the same cue; subsequent restore/settlement refers to this episode. Settlement is current only if its event sequence exceeds the latest same-cue loss. |
| Any observation | Pause, source close, changed request/binding, active lease, pending effect/inbox, storage/event failure | Existing guards prevent or roll back the handoff. Supported terminal retirement remains authoritative; retained facts do not authorize new work. |

The latest same-generation loss record must match the current complete checkpoint
and authenticated cue. A restoration consumes that particular loss sequence.
These event sequences distinguish actual loss/restoration episodes; a wall-clock,
heartbeat or code hash is not a dependency change. Publication readiness is an
actual filesystem check, not a claim that source bytes have changed. Its incomplete
coverage below must be resolved before activation can claim unchanged-failure
suppression across all setup destinations and publication operations.

### Shared bounded fair selection

Negative, positive, native and verifier-infrastructure holds use the existing
shared selection window, not separate queues. It admits at most four concurrent
candidates and excludes active workflow IDs, leased rows, future eligibility and
owner pauses. The query scans at most 128 rows ordered by least recent persisted
same-binding `qualification_recovery_selected` event, then workflow ID. Eligible
rows with no prior reservation sort first. A reservation commits before remote
lookup and detailed source eligibility checks, so invalid/inaccessible candidates
also rotate instead of retaining the front of the window. The same binding has
a persisted 300,000 ms inspection cooldown. Restart reads those same events;
no in-memory reset, generation or day boundary replenishes model allowance.

The matching selection binding consists of head, contract and evidence hash.
At most four selected inspections run, while skipped invalid rows may receive
bounded reservations within the 128-row scan. Positive and negative handling
share the same candidate execution; negative-source authentication stays first.
The existing free physical-custody restoration route stays ahead of optional
positive inspection. If no retained cue exists, inability to inspect optional
original data cannot replace the existing exact original-plan/hold/owner/lease
rules for physical fallback. A known retained continuation never grants original
re-execution merely because its source becomes unavailable.

### Current status and merge blockers

| Earlier thread | Current contract/evidence | Remaining obligation |
| --- | --- | --- |
| Citation repair envelope / checked receipt | D1–D3, §§14–17; parser, complete measurement/native reader and actual qualifier fixtures | Final joined validation, engineering review and ordinary acceptance. |
| Positive cue / legacy separation | Cue schema and transaction rules above; typed v2 store tests, v1 rejection and stable-key restart tests | Fresh seam audit and final production-path verification. |
| Live rebinding / initial and enrolled consumers | Binding/consumer rules above; actual claim, preparation, qualification, publication and settlement fixtures | Final current-main integration and exact-head review/CI. |
| Loss, duplicate, interruption and settlement | Episode table above; atomicity, source-loss and restoration store/guard fixtures | Publication/setup coverage below and natural ordinary outcome. |
| Bounded fair scheduling | Shared window rules above; scheduler rotation and refused-inspection tests | Fresh seam review; do not infer live recovery from selection counts. |
| Publication destination and metadata operations | Current probe covers the receipt/controller directory's allocation/write/fsync and exact expected receipt collision | **OPEN, blocks merge/activation:** enumerate and validate every actual setup destination, including missing harness parents and literal write-admission storage. §22 replaces the anonymous-only check with actual bounded no-clobber publication and owned restart cleanup. Validate that refinement and cover the other setup destinations before claiming full setup readiness. Root owns the remaining implementation. |
| Physical recovery compatibility | Existing free custody restoration is ordered before optional positive inspection; original-plan fallback retains independent authority checks | Verify the connected routing and errors on current code, including incomplete optional positive data; do not broaden re-execution authority. |
| Documentation and fresh review | This section supersedes historic open-status statements and specifies current cue/binding/episode/fairness rules | Fresh-reader findings require recheck; the mapped 100 seams have **not** all been audited. Spec audit is not independent engineering approval. |
| Rollout and full #1703 | Protected merge, hosted CI, supported snapshot and normal updater; natural ordinary workflow acceptance | Entire requirement remains open. No forced resubmission, fabricated input/approval, budget reset or repair self-test. |


## 22. Real controller publication operation and bounded restart cleanup

D11. Replace §20's anonymous-only allocation approximation with the actual
`NamedTempFile::persist_noclobber` operation used by the immutable receipt writer.
Successful anonymous allocation does not establish successful named create or
publication. This refinement covers the private controller directory; it does
not silently establish readiness of missing harness parents or the separate
literal write-admission store. Those remaining destinations stay merge blockers.

The operation holds a nonblocking exclusive descriptor lock on the actual private
controller directory. It creates no permanent lock file. Directory type,
controller UID, private permissions and writable owner bits remain required.
The lock is explicitly unlocked on return/error and released by the kernel on
process death. Explicit unlock prevents a transient pre-exec fork that inherited
the open description from extending the completed probe's ownership. A live
competing probe refuses inspection rather than deleting its active scratch.

Two fixed reserved names, `.jarvis-recovery-publication-stage` and
`.jarvis-recovery-publication-final`, are owned solely by this protocol under the
existing protected workflow-control directory. The marker is the exact bytes
`jarvis-publication-readiness-v1\n`. Scratch contains that marker followed by
bounded kernel-random padding, not a receipt, native response, registration,
owner request or qualification. Noncompressible padding avoids inferring actual
allocation from zero-fill on a compressing filesystem. The size bound is
4 MiB + 192 KiB + marker length per file. Only these two names can be reconciled;
no glob, arbitrary temporary-file sweep, snapshot deletion or storage-floor
exception is added.

Before another probe allocates, it reclaims interrupted state while holding the
same directory lock. Each present reserved file must be a nonblocking-opened,
no-follow regular file, owned by the controller, private and bounded. Its prefix
must match the full marker or a valid partial marker. A two-link file is accepted
only if both reserved names account for that same device/inode pair, covering an
interrupted hard-link-based no-clobber fallback. An external/unaccounted link,
FIFO, symlink, foreign UID/mode, oversize file or changed marker stays untouched
and causes a refusal. Path/inode identity is checked again before removal.
Unrecognized contents never become proof that deletion or workflow dispatch is
permitted.

After writing and syncing bounded scratch, the probe publishes only to the
reserved final probe name using no-clobber persistence, syncs the directory,
removes its recognized probe state and syncs the directory again. It never uses
an actual artifact destination as scratch. A concurrent unexpected final-name
conflict refuses publication without replacing that file. Ordinary staging
errors use the publisher's normal temporary-path cleanup; process interruption
leaves at most the bounded reserved state, which the next locked inspection can
reconcile. If a workflow never resumes, at most these two fixed bounded names
remain in its protected control directory. This patch does not implement
general inactive-workflow cleanup. Root owns that unfinished #1703 retention
contract described in §17; #1799 covers decomposition and does not supply it.
No cleanup of original evidence, pending effects, paused workflows, deployment
snapshots or charged custody follows from a probe being stale.

Tests exercise interrupted owned staging, actual child-process exit with no
Rust destructors and a two-name hard-link interruption, active lock refusal,
ENOSPC during writing, FIFO and unowned-hard-link refusal, changed/private-state
checks, and an actual no-clobber final-name conflict after allocation succeeds.
The original receipt remains unchanged. Success leaves no scratch or lock file;
ordinary pre-publication errors clean staging through the normal temporary-path
owner. A failure after persistence can leave the bounded recognized final name
for the next locked reconciliation. The initial-issue/PR preparation and
same-cue source-restoration fixtures also pass on this branch; this is component
integration evidence, not deployed acceptance.

This closes the named-operation design gap for the controller destination only.
It grants no native/model allocation, productive lease, merge or deployment.
Source/native/environment inspection and the explicit live inventory stay
read-only; this separate filesystem readiness phase retains its limited side
effects. The other setup destinations in §21, final current-main integration,
complete seam and engineering reviews, hosted CI and protected rollout remain
open. No broader autoship restoration or ordinary merge is claimed.


## 23. Write-admission destination readiness and actual setup inventory

D12. Share the literal write-admission writer's planning logic with recovery;
do not invent its filename, serialization or existing-record policy in a second
component. `autoship_write_scope::registration_publication` returns the exact
canonical destination, expected v1 JSON bytes and an observed `registered` flag.
This is a read-only, non-serialized controller API, not a model response, grant
or cached authorization. An existing record must pass the normal private-file
reader and equal the expected canonical worktree, contract, allowed paths and
pinned paths. A changed contract or malformed/nonprivate/symlink record refuses
inspection. Missing state is reported without creating any directory or file.

The normal writer uses this same plan. Existing matching registration is
idempotent; pending registration uses the same bytes and no-clobber publication.
The existing <=256 allowed/pinned entry bounds and literal-relative-path checks
are preserved. Serialization is bounded to the existing 1 MiB reader envelope.
An existing ancestor is resolved before appending missing directory components,
so a missing final record cannot conceal a parent redirect into the candidate.
The resolved destination must remain outside that candidate; the recovery caller
also checks every existing worker-writable root, even for a matching existing
registration. No current role, workflow/model budget or qualification is inferred
from the preview's `registered` flag.

Actual preparation and recovery now share `write_admission_inputs`: the original
contract's allowed-path order/directory bits and the proposal's pinned-file order
are not reconstructed differently by the two callers. During readiness, a matching
existing record requires no new registration write. For a pending record, the
separate filesystem phase prepares only its controller-owned parent directories
and runs §22's real no-clobber scratch probe at that destination, bounded by the
expected registration bytes plus 64 KiB. It creates no actual registration or
native write grant. A concurrent final record must be private and byte-identical;
otherwise readiness refuses without overwrite. Actual registration remains the
normal guarded preparation writer's responsibility.

Pending parents must be literal absolute, nonredirected, owned, writable control
storage without group/world write access. Existing 0755 parents are permitted;
record and probe files remain private. Missing directories are created with mode
0700. The shared normal writer also creates new controller directories with mode
0700, avoiding the inherited group-writable umask found by the lifecycle fixture.
No existing directory is silently chmodded and no unsafe parent is treated as
ready. The deployed admission directory was inspected at mode 0700; permission
policy was not weakened to make the tests pass.

The test-first preview regression required an exact pending destination without
side effects; its output now matches the actual writer's bytes. The filesystem
regression reproduced an unwritable admission destination being treated as ready.
Core tests cover read-only preview, idempotent existing state, changed-contract
refusal, private new directories and missing-parent redirects into the candidate.
The initial-issue and PR lifecycle fixture adds corrupt admission state in its
explicitly verified disposable state directory and proves that the real store
callback refuses queueing with unchanged checkpoint/events and original bad bytes.
Only that fixture state is restored for subsequent healthy preparation.

Current verification: full core library **746 passed / 0 failed / 16 explicit
ignores**; CLI publication checks **7 passed / 0 failed**; original witness and
issue/PR lifecycle group **27 passed / 0 failed / 2 explicit ignores**. The explicit
read-only actual #1830 probe passed again with its original Medium decision,
unchanged workflow/events and twelve historical model calls. It calls only the
read-only registration planner, not its directory/probe/writer phase.

The actual source inventory now establishes the next concrete requirement:
**#1830 has no current registration, and its single proposed harness file is
missing from the retained checkout.** The admission destination is now covered;
missing-harness readiness still blocks activation. Trace the actual installer
and parent filesystems next. Do not create persistent probe/bookkeeping files
inside candidate or broadly exempt candidate files from `git_binding`: that
binding includes tracked changes and untracked evidence. Preserve exact existing
harness bytes and original provenance, do not overwrite a tampered verifier, and
ensure interruption cannot strand unexplained probe files before binding checks.

This section supersedes the literal-registration portion of §§21–22's open
coverage status. It does not close the harness destination, current-main merge,
final joined checks, full spec/seam and independent engineering reviews, hosted
CI, protected rollout, natural ordinary acceptance or the complete #1703 goal.
No positive workflow was queued or model call spent by the actual-source probe.


## 24. Missing-harness readiness without candidate-side probe files

D13. Keep actual harness installation in normal guarded preparation. Recovery
readiness must not install executable code, change registered harness bytes,
leave named probes in the candidate or exempt files from `git_binding`.
Both the installer and readiness now use the same parent-path validation and
bounded existing-file reader. Only declared empty parent directories may be
created during readiness; Git does not record them as source evidence.

Every proposed pin must retain the normal dedicated harness-path rule, <=512 KiB
per file, <=256 registration pins and <=4 MiB aggregate prepared-data envelope.
Parents must be real directories without redirected ancestors. An existing pin
is opened no-follow/nonblocking and must be a regular file with exact expected
length and bytes. Changed, dangling or nonregular pins refuse inspection without
overwrite. Candidate tampering retains its distinct diagnostic; a baseline name
collision still identifies its relative path and differing contents so the
independent author can choose a new name rather than repeat an opaque failure.

Missing files are grouped by their actual parent directory. Each parent must
be owned and writable; ordinary repository group-read/write modes are not
mistaken for private controller-directory policy. On supported Linux filesystems,
readiness opens O_TMPFILE in that exact parent and writes/syncs bounded anonymous
scratch for the missing bodies plus metadata margin. It creates no target or
probe directory entry, no permanent lock and no new native registration. The
kernel releases the allocation on process death. Missing parent-directory
creation uses the real installer's path/mkdir logic, so those permission and
redirect failures occur before queueing. Unsupported unnamed-allocation storage
stays held rather than silently using a candidate-side named fallback.

This tests current allocation/quota/write/fsync availability and existing pin
integrity. It is an advisory readiness observation, not a reservation or a
substitute for the real create-new writer and subsequent original-integrity
check. Filesystem state or a target can change afterwards; normal preparation
must still refuse overwrite and preserve the restrictive same-cue failure path.
No claim of arbitrary filesystem-policy support or completed fault/soak
acceptance follows from this preflight.

The test-first regression reproduced an unwritable missing-harness destination
passing readiness. Tests now cover actual permission refusal, injected ENOSPC,
healthy missing parents without installing code, exact-byte installation by the
real writer, changed-verifier preservation, redirected parents and dangling pins.
The Git status before and after successful/failed allocation is unchanged. A
separate child exits while anonymous scratch is open, without Rust destructors;
restart still finds no candidate-side file or changed Git evidence and can probe
again. The normal source/store callbacks use this readiness after the controller
and literal-registration checks, before queueing a productive claim.

Final joined CLI verification for this branch checkpoint: **2,097 passed,
0 failed, 42 explicit ignores**. The previous full core result (746/0/16) remains
for unchanged core code. The actual #1830 compatibility inventory is a separate
explicit read-only check, never this mutating directory/allocation phase.
Current-main integration, fresh complete seam review, independent engineering
review, exact-head hosted CI, supported snapshot, protected merge and updater
rollout remain required. Natural ordinary shipping and the complete #1703
acceptance remain unproven.

The next source-comparison checkpoint was `qualify_original_baseline`, which
created an original temporary worktree even for retained-data recovery. The
decision and implementation status for removing that dependency are in §25.

## 25. Retained-data qualification without a second original checkout

D14. Only an existing authenticated V2 retained-witness reference may take the
data-only path before original-worktree creation. It must pass the normal live
source, owner, lease and dispatch checks, candidate harness integrity, immutable
original environment reconstruction, full captured original result and complete
native question/provenance revalidation. The actual receipt writer remains
`requalify_retained`, followed by the normal `assert_current` reader; there is no
second approval mechanism. Missing, corrupt or incompatible retained data stays
held under the same cue and never falls through to fresh witness generation.

Why: this path reads and revalidates completed original observations; it neither
executes the candidate nor asks a new witness. Requiring a second checkout adds
Git shared-metadata writes and a full-tree allocation without strengthening those
facts. Original captured head and environment are still mandatory. A later
candidate head or candidate compiler-file edit cannot replace original facts:
source and environment come from the immutable original base plus the original
registered proposal overlays, and current tools must match the original
environment identity. This is not permission to run an old measurement in the
candidate environment or certify current candidate behavior.

No-reference, fresh and legacy qualification retain their existing original
checkout path. In particular, `baseline_witness::qualify` still checks actual
checkout HEAD, harness integrity and installed environment before generating any
new native witness. Missing original setup in those paths remains a real failure.
Current independent review, candidate GREEN, CI and merge requirements are
unchanged. Data-only recovery cannot itself authorize a builder until actual
prepared publication and the existing guarded settlement succeed.

Implementation checkpoint: the real `qualify_original_baseline` caller now uses
`qualify_known_retained` before creating a worktree. A regression for both issue
and PR sources advances the
candidate HEAD, changes a candidate compiler input and makes Git worktree
creation unavailable. It proves successful requalification without a new
checkout or native admission, while direct fresh qualification still refuses
the candidate HEAD. The joined witness checkpoint passed **27 tests, zero
failures, two explicit read-only ignores**. Changed-harness and lost-native-proof
checks at this production caller also passed in the full joined CLI run:
**2,097 passed, zero failures, 42 explicit ignores, 374.77 seconds**. This is branch
implementation evidence, not deployment or ordinary acceptance. §26 remains an
explicit final-handoff/activation blocker.
The final explicit read-only #1830 inventory passed in 11.26 seconds with its
snapshot/events and twelve historical model calls unchanged; it published no
qualification and queued no work.

## 26. Fresh-audit liveness finding and branch resolution

The fresh-reader audit of §§1–25 identified one unresolved liveness contract:
advisory readiness may pass while a later real writer repeatedly fails the same
operation. A stable cue prevents another initial grant, but restoration episodes
alone do not prove changed setup dependencies. The five-minute inspection
cooldown controls rate; it is not a dependency-change predicate or a complete
bound on productive claims. The new data-only path in §25 removes the full
original-checkout dependency, but did not by itself resolve this general writer
gap. D15–D17 below now implement the dependency/completion and checkout handoffs;
their branch validation checkpoint is recorded after §29. Final independent
review, complete spec recheck, hosted CI and rollout remain required.

The current source comparison identifies these real operations:

| Real writer | Readiness evidence | Remaining distinction |
| --- | --- | --- |
| `harness_parent`: validate/create declared directories | The same helper runs during harness readiness | Intervening directory/permission changes still require the normal restrictive failure path. |
| `install_harness`: named `create_new`, body write and file sync | Anonymous allocation/write/sync in the same parent; exact existing-pin read | The probe does not perform named creation. A partial actual write leaves a differing pin that subsequent source/readiness checks refuse without overwrite. A refusal before creation may leave no differing pin. |
| `autoship_write_scope::register_at`: private parent creation, temporary file, byte write, sync and no-clobber persistence | Shared destination/bytes plan plus named no-clobber probe in that parent and concurrent final-record validation | The probe uses reserved names; the normal final record and any intervening changes still require actual writer validation. |
| `persist_initial_prepared` and `write_manifest`: temporary file, exact byte write, sync, no-clobber persistence and controller-directory sync | The controller probe exercises named no-clobber persistence and directory sync with the combined prepared/manifest/receipt allocation | Actual artifact names remain distinct. Existing prepared/manifest state is independently validated by retained-source inspection. |
| `baseline_witness::immutable`: exact existing receipt or temporary file, write/sync, no-clobber and directory sync | Same controller publication operation plus exact expected-receipt conflict validation | Current live qualification and the actual immutable writer remain mandatory; readiness is not a receipt or approval. |

This inventory narrows the next regression to an unchanged failing real operation
whose readiness continues to pass. It does not establish a general guarantee
against concurrent filesystem changes or unsupported per-name security policy.

Root owned the test-first resolution before final handoff/activation:
enumerate real writer operations against the actual readiness operations;
reproduce repeated successful readiness with an unchanged failing real writer;
then define and implement the durable operation-specific change predicate or
explicit bounded retry policy. Verify claims, attempts, model calls and events
across restart, with legitimate resource/source restoration still able to recover
without changing the original owner request, resetting charges or inventing
new original data. No hidden generic retry or merely relabeled hold is an exit.
These mechanisms are implemented and tested on the branch below. No remaining
review or rollout obligation is delegated to the owner as a product question.

The latest map identifies **105 seams** across §§1–25. Mapping is not checking.
Bounded read-only audits and root recheck currently cover **S61–S105: 45 checked,
45 holding**. S1–S60 remain unchecked against this latest map; the new
writer/readiness contract and any affected seams must also be checked after its
resolution. These counts do not close the separate fresh-reader liveness finding.
No independent engineering approval,
hosted CI result, snapshot, rollout or natural autonomous merge is implied.

## 27. Operation-specific setup dependencies (implementation in progress)

D15. A real retained-setup I/O failure must retain a bounded dependency record
atomically with its same-cue restrictive checkpoint. The record contains protocol
version 1 (quota exhaustion uses the explicit version-2 amendment in §30), the operation (`prepared`, `manifest`, `harness`, `registration` or
`qualification`), condition class (`permission`, `capacity` or `other`) and an
optional SHA-256 filesystem fingerprint. A capacity condition also carries
bounded available-block/inode observations keyed by opaque namespace hashes.
It contains neither raw private paths
nor an authorization grant. The existing loss event advances to version 2 only
when this field is present; version-1 source losses keep their existing semantics.
Malformed records and unsupported operations/versions remain held.

The fingerprint covers the operation's actual setup namespace, the literal path
and ancestor existence/type/device/inode/owner/group/mode and applicable ACL and
security labels. It excludes directory timestamps and probe bookkeeping so the
inspector cannot manufacture a change by running itself. Current filesystem
read-only state and process access identity are relevant. Only capacity failures
add available blocks/inodes and affected directory-entry counts/name-byte totals;
unrelated
free-space changes must not wake a persistent permission denial. Existing source,
environment, scope, integrity and real readiness checks are still prerequisites.
No source/build hash, clock, generation, owner revision or day enters this record.
Use an explicit versioned deterministic encoding; future code edits alone must
not change its meaning or refill any existing allowance.

Operations share their actual destinations: harness paths/parents come from the
registered original proposal, registration from the normal publication planner,
and prepared/manifest/qualification from the protected controller directory.
Only bounded metadata, ACL/label bytes and, for capacity failures, entry names
are read; no source code is executed and no setup artifact is published by this
inspection. Unobservable dependencies are recorded as an absent fingerprint;
current inspection must produce a complete observed fingerprint before queueing.
Bounds: at most 256 targets, 8,192 distinct path observations, 256 process groups,
16 KiB per security attribute and 8 MiB aggregate security-attribute reads.
Read only `system.posix_acl_access`, `system.posix_acl_default` and
`security.selinux`, hashing their bytes rather than storing them. A capacity
inventory is nonrecursive, at most 8,192 entry names and 4 MiB of aggregate name
bytes across the actual setup parents. The full deterministic encoding is at
most 4 MiB. Refuse redirected namespaces and incomplete/over-bound observations.
This does not authorize a broader metadata sweep, candidate-side probe files,
new registration grants or moving guarded setup publication into free polling.

The store—not the scheduler cooldown—enforces the recovery predicate inside the
existing expected-snapshot/owner/pending-effect transaction. A waiting setup loss
requires the same operation and condition and a complete current observation.
A namespace/access fingerprint change is eligible for current validation. For a
capacity condition, increased available bytes/inodes or fewer entries/name bytes
under the same namespace identity is also eligible. Merely consumed space,
added entries or an arbitrary equal-size rename cannot
restore a claim: writing its own diagnostic or running a probe must not manufacture
recovery. Capacity counters are therefore compared structurally and excluded
from the namespace fingerprint; entry names are never part of that hash, and
the two reserved readiness names are excluded
from the directory-entry inventory. This observation grants nothing without all
normal source/readiness checks. The legacy reference-only
callback supplies no such proof and cannot bypass the predicate. The callback
still authenticates the original reference and passes all current readiness
checks. Unchanged/worse conditions, unknown current state or mismatched proof queue
nothing and changes no checkpoint, generation, calls, tokens or loss events.
An observed dependency becoming readable after an unobservable failure is a real
change; it is not an invented owner-input edit. A later failed operation records
its newly observed conditions rather than replenishing a retry allowance.

Why: source bytes can remain identical while storage permissions or capacity
are repaired. Requiring source edits would strand legitimate recovery, while an
advisory success alone would permit unchanged failing writers to loop. Relevant
filesystem changes are necessary but not sufficient; current source/readiness
validation and the actual live-fenced writer still decide whether work proceeds.
Arbitrary external per-name security-policy changes that have no observable
filesystem manifestation are not inferred from time or a new binary. Such a
case remains explicitly held until a genuine observable filesystem/ACL/label or
mount repair supplies new evidence. No workflow force-resume override, artificial
input change or fresh metadata/model allowance is introduced for it.

Implementation order: first reproduce unchanged advisory-success recovery in a
reopened store, including the legacy-callback bypass; then add atomic loss/proof
validation and negative transaction tests; then capture real writer failures,
implement bounded filesystem observations, wire production inspection and test
actual issue/PR preparation across unchanged failure and legitimate repair.
The root agent owns the implementation and revalidation. The branch checkpoint
below records the current exits; no review, deployment or ordinary-acceptance
claim follows.

## 28. Frozen dependency encoding and terminal setup observations

D16. Freeze the v1 fingerprint bytes as compact `serde_json` UTF-8 serialization
of this ordered tuple, then lowercase SHA-256:

`("retained-setup-filesystem-dependencies-v1", operation, condition, access, targets, nodes, spaces)`.

- `operation`/`condition` are the exact lower-case strings in D15. No source or
  executable-version hash participates.
- `access` is an object in declaration order `uid`, `gid`, `groups`: unsigned
  effective user/group IDs and sorted/deduplicated supplementary group IDs.
- `targets` are literal absolute UTF-8 path strings, sorted by UTF-8 string order
  and deduplicated. They are the actual D15 operation namespaces, not source
  citations or a different staging location.
- `nodes` is a BTreeMap ordered by absolute UTF-8 prefix path. Every target
  ancestor is represented. Missing paths are JSON `null`; existing nodes have
  fields in order `dev`, `ino`, `uid`, `gid`, `mode`, `size`, `labels`. `mode`
  includes Unix type bits; `size` is unsigned length for a regular file and null
  for a directory. Symlinks/refused traversal and unreadable metadata produce an
  incomplete observation, never a fabricated missing state.
  The qualification namespace additionally has the fixed logical key
  `@qualification` for metadata of its actual current receipt target. Its
  generated context/policy filename is not encoded. This observes an actual
  target permission/removal repair without inserting a checker/build hash into
  the dependency protocol; two absent targets remain the same observation even
  if an unrelated build selects a new current context.
- `labels` is the fixed ordered array of `(name, hash-or-null)` pairs for
  `system.posix_acl_access`, `system.posix_acl_default`, `security.selinux`.
  Hashes cover exact attribute bytes. ENODATA and ENOTSUP mean null; other errors
  or changing/oversized reads make the observation incomplete.
- `spaces` is a BTreeMap from each nearest existing real setup-parent directory
  to its unsigned read-only mount flag (`statvfs.f_flag & ST_RDONLY`). Capacity
  counters and directory-entry values are excluded entirely from these bytes.
  Directory times/link counts/length, unrelated files and probe execution do not
  participate.

The synthetic `/fixture/case.py` codec vector in
`setup_fingerprint_encoding_keeps_a_frozen_data_vector_without_code_or_clock`
pins exact JSON bytes and SHA-256
`a8be73b4c4cb6e0c50b1c71010115fe035095fa13bc287d642769d9cb17aa6de`.
The dependency object has fields in declaration order `version`, `operation`,
`condition`, `fingerprint`, `capacity`, `completed`. Unobservable fingerprints
and inapplicable capacity are JSON null, not omitted or replaced with zero.
All private persisted readers reject unknown fields/unsupported versions;
older source-loss version 1 omits both `setup` and `setup_candidate`.

Capacity observations are a separate BTreeMap. Its opaque key is SHA-256 of the
compact tuple `("retained-setup-capacity-namespace-v1", absolute_parent_path)`.
Each value's field order is `available_blocks`, `available_inodes`, `block_size`,
`entries`, `name_bytes`; all are unsigned integers. Available bytes are compared
as blocks × block size using u128. A block size must be 1–1,048,576; the D15
namespace/entry bounds apply. Improvement means at least one available-resource
increase or entry/name-byte decrease, still subject to all current readiness
checks. A new entry, equal-size rename, or diagnostic consuming space is not an
improvement. The reserved stage/final readiness names contribute neither entries
nor name bytes. Namespace sets must match for a counter-only comparison; a real
namespace/access change is independently visible in the fingerprint.

An observation also has a controller-derived `completed` boolean, initially
false in every failed-operation record. Current recovery may set it true only
when that actual writer's normal idempotent exit is already established:

| Operation | Read-only completion observation |
| --- | --- |
| Harness | Every declared pin passes the same bounded exact-file/parent checks used by installation; no missing pin remains. |
| Registration | The normal shared publication planner reports the complete expected private registration already present. |
| Prepared | The normal retained-prepared reader yields the complete exact original prepared bytes. |
| Manifest | The actual private manifest exists and exactly equals the normal expected manifest, including its validated evidence-file selection. Never call the orphan-recovering manifest writer from free inspection. |
| Qualification | The actual current qualification reader succeeds, including its complete original/native evidence checks. |

Why: an I/O failure can happen after valid bytes were published. Requiring an
artificial metadata change would strand a writer that now returns success without
writing. Complete observed state, a genuine dependency change, or genuine capacity
improvement can restore this same cue. All require a complete current observation
and all source/readiness fences; no boolean supplied by a model or disk preview
establishes completion. Actual prepared publication, current qualification and
guarded settlement still decide builder authority. The restored event retains
the exact observed setup proof for audit. The failed record cannot claim its own
completion or replace an existing waiting dependency.

## 29. Preserve partial setup through the real checkout boundary

D17. Keep the existing exact Git-input fence for ordinary, negative/native and
physical re-execution recovery. A retained setup failure may additionally retain
one exact full candidate binding observed by its live healthy guard after the
failed operation. It is stored atomically in the same version-2 loss event as
`setup_candidate`, not in a second ownership ledger or in the stable cue key.
The recorded head and contract must equal the lease/checkpoint head and contract;
only evidence may differ after local setup. Git observation failure stores no
invented binding. Later free inspection cannot replace this record.

This amends §21's binding handoff only for that authenticated positive setup loss.
The real poller still checks exact registered checkout/root ownership, branch or
canonical detached-PR identity, repository, remote publication/head ownership and
the unchanged protected owner request. If current Git inputs differ from the old
checkpoint, they must equal the complete `setup_candidate` from that exact waiting
loss. A new head, later edit, missing observation, different branch/checkout or
another workflow's record is refused. This preserves already-written pins without
ignoring them in Git evidence or broadly accepting dirty worktrees.

Only the retained-data inspection route can use this recorded binding. Negative
semantic recovery and physical native/original-plan re-execution keep their exact
binding rules and cannot borrow it. The positive queue transaction still compares
the original expected snapshot/owner/pause/pending-effect state, authenticates all
original data and setup proof, and creates no native allowance. On the normal next
claim, existing guard acquisition rotates the current binding/audit and records
the protected recovery context before consuming the same cue. Candidate tests,
review and hosted CI remain bound to the actual later candidate revision.

Reason: a real guarded setup operation can write pins before a subsequent failure.
Its full checkout evidence then differs from the earlier checkpoint. Rejecting
that observed state forever would strand the repair; accepting arbitrary current
changes would weaken custody. Retaining the exact failure-time observation closes
that handoff while keeping later changes outside its authority. This is still
implementation/validation work, not an authorization to deploy or a claim that
full fault/soak acceptance passed.

Branch validation: the unchanged-writer/restart regression first failed because
advisory success requeued the same conditions. The capacity-repair regression
then exposed missing structural capacity comparison. The completed-operation
and actual linked-checkout regressions both failed before their corresponding
fixes. Those cases are now green, along with malformed/mismatched/atomic-loss
checks, bounded metadata/redirect tests, exact codec bytes, the issue/PR
preparation → loss → unchanged inspection → real condition change → normal
qualification/settlement chain, and the later native-source restoration cases.
No injected failure or scripted native fixture is live acceptance evidence.

A final private-diagnostic regression reproduced an immutable-key collision
when the same error recurred after an owned binding rotation. The diagnostic
projection now uses domain `semantic-source-diagnostic-v2` with workflow, cue,
the complete binding and diagnostic text. Its body remains diagnostic v1; it is
not a recovery grant or another budget ledger. Repeats on the same binding are
idempotent, while legitimately different bindings retain their own exact record.

Final joined CLI checkpoint: **2,109 passed, zero failures, 42 explicit ignores,
376.80 seconds**. The separate explicit read-only #1830 inventory passed in
**11.34 seconds**, with its complete snapshot/events and twelve historical model
calls unchanged, and no qualification or dispatch. Current remote main remains
`36fcb99ab3de28d9415a84f634e29c609a015a35`, already merged normally into this
branch. These are implementation/source-compatibility results. Final seam
rechecks, independent engineering review, exact-head hosted CI, supported
snapshot, protected merge, normal updater and natural ordinary shipping remain
required. #1703 and #2058 retain their complete outstanding scopes.

Final reader-alignment checkpoint: the retained v2 cue reader now rejects even
an internally consistent rekeyed payload with an unsupported policy; historical
negative v1 policy handling is unchanged. Its test-first negative failed before
the check and the 25 recovery tests passed afterwards. Free initial-scope
readiness and leased replay now share the committed body/event reader. The
issue/PR fixture first reproduced orphan-body acceptance, then passed with that
receipt removed and exact snapshot/events unchanged (194.70s); the three scope
pair custody/restart tests also passed. These focused results supplement, rather
than replace or rerun, the preceding 2,109-test full-suite checkpoint. Independent
review's quota-observation finding remains open; specification checks are not
engineering approval or activation evidence.
The stricter read-only actual #1830 inventory also passed (11.34s): committed
original scope remains Medium with no fallback; snapshot/events and 12 historical
model calls are unchanged; no qualification, grant or dispatch occurred.

## 30. Quota repair is a separate setup dependency

Independent review found that §27's filesystem capacity observation cannot prove
all quota repairs. `EDQUOT` now uses setup-dependency version 2, condition
`capacity`, with a separate optional `quotas` map; ordinary `ENOSPC` and other
conditions retain version 1 and omit that field, preserving their encoding.
Version 2 requires a complete quota map whenever its filesystem fingerprint is
present; unknown observations have no fingerprint, capacity or quota map.
A v1 proof cannot restore a v2 quota loss. Existing cue, source-loss and budget
identity are unchanged; no new model or metadata allowance is introduced.

For each actual setup parent, observe Linux quota enforcement with
`quotactl_fd(Q_XGETQSTATV)` and the applicable enforced user/group/project limits
with `Q_XGETQUOTA`. These are quota read commands, never quota setters. The
ordinary `Q_GETQUOTA` command is deliberately not used: Linux classifies it as
requiring mount write access. Native UAPI structures are version 1. The namespace
key is SHA-256 of compact JSON
`["retained-setup-quota-namespace-v1",literal_parent_path,kind]`, with numeric kind
0=user, 1=group, 2=project, three entries per observed parent, at most 768 total.
The charged user is effective UID; group uses the actual setgid or
`grpid/bsdgroups` inheritance policy, otherwise effective GID. If parent and
process GIDs differ without setgid, ext4 uses the per-device `/proc/fs/ext4`
full options view (mountinfo can omit filesystem defaults); XFS uses the exact
open descriptor's mount ID and its mountinfo superblock options. Reads are
bounded to 64 KiB per options/fdinfo view and 4 MiB for mountinfo. Missing,
ambiguous or unsupported ownership policy stays unknown. No scratch inode is
created to guess its quota owner. `FS_IOC_FSGETXATTR` supplies inherited project ID (otherwise zero) and refuses
realtime allocation, whose counters are outside this normal-data contract.

Each strict quota record contains `id`, `enabled`, `space_limit_blocks`,
`space_hard_blocks`, `space_used_bytes`, `inode_limit`, `inode_hard_limit`,
`inodes_used`, `space_grace`, `inode_grace`, in
that declaration order. Space limits use 512-byte units, usage uses bytes,
inode values are counts and grace values are recorded Unix deadlines, not the
current clock. The combined limits select the smaller nonzero hard/soft limit; the
separate hard limits preserve increases during a soft-limit grace period, when
the smaller soft limit alone cannot describe available hard-quota headroom.
Zero means unlimited. A nonzero hard limit bounds a positive combined limit. Disabled entries are explicit `enabled:false` and all-zero fields.
Unavailable/denied/unsupported quota queries remain unknown, never disabled.
Linux's no-enabled-quota `ENOSYS` result is disambiguated only when all three
`Q_GETINFO` reads positively report inactive (`ESRCH`).

Under the existing complete source/readiness/store fences, a same-fingerprint
quota loss can restore only for increased applicable combined or hard-quota
headroom (actual full readiness still precedes comparison), observed
removal of enforcement, a changed charged identity, an extension of an already
recorded grace deadline, or exact read-only operation completion. Filesystem
free-space and directory-entry changes alone cannot release a quota hold.
Unchanged/worse quota observations, missing proof and downgraded proof preserve
checkpoint, history and charges. Actual publication still has its own live
lease/binding guards and can fail; a new real failure preserves its own observed
state and the same cue. This does not claim support for unobservable server-side
quotas, realtime allocation or unavailable kernel APIs; those remain explicit
infrastructure holds rather than fabricated recovery or unchanged retry loops.

Quota validation checkpoint: test-first EDQUOT/global-capacity and BSD-group
regressions reproduced the defects before correction; all eight quota-filter
tests then passed. The managed full CLI run passed 2,115 tests, zero failures,
42 explicit ignores (378.61s). The separate read-only actual #1830 inventory
passed (11.27s): all three quota kinds observed, none enforced; complete original
scope/native/capture evidence, snapshot/events and 12 historical model calls
unchanged. That host observation does not claim an active-quota integration test
or a restored autonomous merge. Independent new-head review, hosted CI, supported
snapshot and protected rollout remain required.

A final test-first projection regression reproduced loss of a hard-limit increase
behind an unchanged soft limit. Separate raw hard limits now preserve that repair
through the native observer and same-cue comparator; invalid combined/hard
relationships remain unavailable. The final focused run passed nine quota tests,
all 26 recovery-store tests passed (1.20s), and the read-only actual #1830 probe
passed again (11.46s) with unchanged scope/evidence/history/12 model calls.
The 2,115-test full run above is the preceding `36317efa` checkpoint; the final
hard-limit amendment requires its own exact-head hosted CI and review.
