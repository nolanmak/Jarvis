# Verifier construction before adjudication (#2058)

Status: draft under source comparison. The production implementation is not
complete. Open decisions below block implementation handoff and rollout. This
does not replace the acceptance requirements in #1703.

Fresh read-only review on 2026-11-13 found the episode-identity/reset and proposal
slot-accounting findings resolved by section 3. That narrow review does not
approve the remaining producer, admission, staging or fixture integrations.

## Evidence and scope

The amendment path currently asks an independent authority to construct and
approve a replacement in one response. It records a complete refusal before
constructing or measuring any replacement. Complete original source supplied to
ordinary #2014 did not produce a replacement. Its completed decline, original
request, candidate and 27 recorded calls remain preserved.

An opt-in probe compiled an observer against original commit `1d390c41`, through
the existing storage admission, immutable exporter and isolated executor. Public
iMessage replier and renderer APIs produced one pending approval card with no
errors or provider calls. No oracle was evaluated. This is partial setup
evidence, not original behavioral red or complete coverage. The exact program is
in `crates/augmentagent-cli/test-fixtures/original-imessage-observer.rs`.

The registered requirements cover every affected iMessage, SocialAPI and LinkedIn
approval-card path; advisory-only creation, approval and sending; valid owner
override suppression; and matching/insufficient evidence suppression. Preserve
every criterion ID and exact requirement, original behavioral base, candidate,
allowed implementation scope, charges, decisions and uncertain effects. A public
card API or public handler trait does not establish approval/send coverage. The
concrete original CLI `ReplyApprover` is private; this does not establish that
every possible observer is impossible.

## Proposed contracts

1. **Separate roles.** An untrusted constructor returns executable proposal data,
   never an approval. Existing isolated staging measures it at the registered
   original commit. Existing original-witness qualification checks attribution
   to production. Independent adjudication then receives that concrete staged
   proposal and evidence. Candidate green, alignment, independent review and
   exact-head CI/merge gates remain later requirements. No new workflow engine
   or command-only execution path is introduced. Record actual constructor
   provider/session ownership. Adjudication must use a different protected native
   session from constructor and candidate authorship through the existing reviewer
   isolation service; provider names or role labels alone do not prove independence.

2. **Fresh-dispatch gate.** A fresh authority call requires a controller-created
   staged-correction value bound to workflow, full current binding, old contract,
   exact replacement proposal, owner requirements, original commit, environment,
   raw measurement and qualified witness. Source excerpts alone are insufficient.
   Missing, conflicting, incomplete or infrastructure-only evidence holds before
   authority admission. Completed existing decisions and pending amendments
   replay through their existing checks without another call. An approved new
   decision must name the exact staged proposal; it cannot rewrite unmeasured
   files. A valid refusal remains a completed refusal.

3. **Explicit construction budget (proposed).** Add at most six actual native
   conversations per construction episode, including source selection and format
   correction, and at most three proposal measurement slots. The canonical episode
   key is the digest of protocol tag `verifier-construction-episode-v1`, canonical
   workflow ID, the typed source from the protected old registered contract,
   registered original commit, old registered contract hash and controller-measured
   original execution environment. Source means `AcceptanceSource::Issue(number)`
   or `AcceptanceSource::PullRequest(number)`, not a selected file/excerpt set,
   URL, branch, provider response or current adoption alias. Canonical workflow
   resolution must not let a PR and its source issue create parallel budgets.
   Main-only movement, UTC rollover, policy-only hash changes, source-selection
   changes, new feedback and candidate binding rotation do not replenish it.
   A replacement episode requires a genuinely activated new registered contract,
   or a trusted change in the original execution environment (compiler, locked
   dependencies or execution-engine policy), measured through the existing
   environment service. Preserve old episode records; a new serialization or
   controller version is not evidence of an execution-environment change.
   Every exact native input has
   at most one admission; a lost response is unknown, not an empty response slot.
   Construction charges remain in the same workflow accounting but in a named
   construction domain. Existing authority limits (two per input, four total)
   and every historical charge remain unchanged. Construction may exhaust before
   three valid proposals exist. No proposal or an exhausted component produces
   a precise durable hold, not another unchanged hourly paid retry.

   A proposal slot is consumed by its first durable measurement intent, after a
   complete proposal passes syntax, source, unchanged-criteria, revision and scope
   validation, and before execution. All files/verifiers in one contract consume
   one slot. A corrected proposal in another generation consumes another slot.
   Malformed output, source-selection output and an explicit no-proposal response
   do not create measurement intents, but their actual conversations stay charged.
   An identical proposal returned in a later generation still consumes that
   generation's proposal slot; reuse its recorded measurement instead of executing
   it again. Replaying a completed slot creates no new slot or conversation.
   Changed source/qualification inputs must be authenticated through the existing
   source service, not inferred from another generation or a different clock.

4. **Custody before release.** Retain completed native output and its actual
   provider/session/request/response correlation through existing protected
   persistence before releasing the native scope. Persist the proposal and
   original execution intent before staging. Reuse completed measurements on
   restart; do not manufacture another witness input by reexecuting unchanged
   code. Capture/quiescence/result checks cover earlier admitted commands too.
   Negative/setup feedback remains private untrusted data. Missing physical or
   indexed custody holds before another paid call.

5. **Original source and compatibility.** Reuse bounded controller-mediated reads
   from the immutable original Git tree and the existing source/provider request
   envelopes. Preserve complete requirements and diagnoses without silent
   truncation. Do not grant native host filesystem/command tools as a shortcut.
   Construction is a distinct purpose, not a baseline-witness approval. Current
   qualification remains current-policy strict. Historical negative judgments
   may be used only as authenticated data; no general old-policy whitelist or
   old positive approval is introduced. Changes to the source producer require
   explicit historical-data compatibility tests and the retained-production
   read-only probe before rollout.

6. **Held work.** Existing free inspection and normal selection may schedule a
   bounded construction component for an eligible invalid verifier. That does
   not clear the canonical acceptance hold or reconsider its completed decision.
   Only genuine new staged proposal/evidence can supply the existing atomic
   recovery cue for subsequent normal adjudication. Owner pauses, closed targets,
   active leases, unqualified implementation edits and uncertain mutations remain
   blocking. General recovery-inspection leases remain unable to dispatch models.
   No fabricated owner edit, command job, counter reset, candidate modification or
   forced resubmission is permitted.

7. **Environment transition.** A provisional original measurement does not approve
   an environment transition. The existing authority still decides that explicitly.
   If approval changes registration identity, old staged receipts cannot simply
   be relabeled: the authorized registration must obtain its required original
   measurement/qualification and separate candidate green. Reuse unchanged proof
   only when its exact registration and current qualification still match.

## Test-first exits

- Reproduce a source-backed complete refusal reaching the real authority-call
  boundary without a staged replacement. The new gate prevents that dispatch.
- Through the same production interfaces, prove proposal creation, isolated
  original measurement/qualification and then independent adjudication occur in
  that order. Preserve genuine refusals and exact proposal identity.
- Exercise setup failure feedback, changed/foreign/missing custody, infrastructure
  recovery, environment transitions, source bounds and all original criteria.
- Prove actual-call limits across binding changes, restarts, missing responses,
  new main commits and UTC rollover. No constructor or qualification failure
  spends an authority attempt. Replay completed output before new admission.
- Prove the saved request/source/proposal handoff read-only. Deliver the complete
  engineering change with normal CI/review/snapshot/merge/updater and both-process
  proof. Use ordinary work for autonomous acceptance, never this repair PR.

## Selected producer interface and custody boundary

The unactivated `amendment::construction` driver now persists exact constructor
inputs before dispatch, reuses saved inputs after model-default changes, and
validates source/revision/requirements/scope before reserving one of three
measurement slots. Its six-round ledger uses construction-specific keys, not the
initial-verifier budget namespace. A repeated complete proposal in a later round
reserves another slot; a pending intent replays unchanged. Completed constructor
refusals remain refusals. Malformed output consumes a constructor round without
reserving a measurement slot. The native transport separately enforces actual
admission and immutable completion custody; driver tests use scripted replies
and do not themselves establish real provider execution.

Infrastructure failures cannot record semantic rejection or advance a slot.
Structurally invalid source requests can be corrected within the same bounded
episode. A structurally valid request whose Git bytes or required excerpt cannot
be read holds before another conversation; this deliberately includes a missing
requested needle rather than treating all read failures as model mistakes.
Exact failed proposals and private diagnoses are included in subsequent requests
without silent truncation. An oversized complete context holds before dispatch.

This driver returns an **unqualified measurement intent**. It is not connected
to production selection, original measurement, witness qualification or
authority admission yet. Those integrations, the restricted component lease and
cross-episode data-only recovery remain required before rollout. The existing
source-only authority-boundary regression remains red. No production constructor
conversation or autonomous merge is established by these component tests.

Use a separate amendment-construction module beneath the existing acceptance
adapter. It is an untrusted producer, not another workflow coordinator. Reuse
`build_workflow_pinned`, tool-free `codex_review_opts`, the existing protected
fresh-session service, `WorkflowGuard` accounting and the immutable paired
scope-response store. Do not route construction through `persistent_witness`
or add construction results to baseline-witness custody. Keeping the existing
scope producer unchanged also avoids changing its full-file policy identity
just to add an unrelated producer role.

Each construction conversation carries the controller-computed episode and
exact-input purpose into the actual admission observer. Its native role is
`verifier_construction`; the runtime's fresh-session isolation marker is a
technical custody fact, not witness approval. The producer returns proposal
data, a bounded request for immutable original source, or an explicit no-proposal
result. Controller validation decides which variant it is. Source and format
responses consume the same episode budget as proposal responses.

The exact input is SHA256 of UTF-8 `serde_json::to_vec` bytes for this ordered
tuple: `("verifier-construction-input-v1", episode, provider_name,
"verifier_construction", producer_policy_digest, model_profile, system, prompt)`.
`producer_policy_digest` binds the compiled constructor module, its bounded
additional-source adapter, native-session service and Codex/Claude adapters.
`model_profile` is an ordered tuple of profile version,
the explicit resolved model name, permission mode, environment restriction flag,
allowed tools, additional directories, extra environment and settings JSON.
For this protocol require the model name to be present, permission mode
`default`, `restrict_env=false` as in the existing review transport,
empty tools/directories/extra environment and null settings. Clear additional
directories inherited from the review preset; fresh scope supplies the worktree
as its working directory only. Session and handoff overrides are null.

The constructor installs `native_session::literal_text_scope` inside the fresh
native scope. Each adapter validates its provider, model, system, user payload
and allowed options against that sealed request before native admission.
The workflow's actual admission observer separately requires the matching
literal-text provider grant for every construction-purpose dispatch. The
general fresh-session marker alone cannot admit a constructor conversation;
the connected regression first reproduced that missing grant check.
Codex and Claude dispatch the specified literal model without router/tier
remapping. Codex preserves the supplied system text without appending workspace
instructions and leaves every `IMAGE:` line as literal text, with no attachment
extraction or local image reads. Existing callers outside this explicit scope
retain their routing behavior. Exact payload means the controller-supplied
system and user text at the native CLI interface; native-session/protocol policy
is separately bound by the compiled adapter/session identities.

CLI-capture tests exercise both adapters with conflicting router
defaults, capture the model argument and system/user payloads, and check that
Codex receives no image attachment flags for an existing image marker. The
initial Codex regression observed the rewritten model and attachment before
the fix. Fresh read-only source review rechecked and resolved both production
adapter findings. Scripted constructor-exchange tests alone do not establish
these adapter guarantees.

Hash the exact system/prompt bytes actually sent, including quoted source and
feedback. Do not hash a current binding, generation, timestamp, transient path,
session ID or main SHA separately. The episode already pins the canonical
workflow, source and original commit. Constructor prompts use those immutable
inputs rather than injecting current-main or candidate-head metadata. A policy
or model change creates a different exact input but never replenishes the
episode's six-call allowance or independently wakes a completed hold.

Retain this complete input envelope with its dispatch intent. Restart recovery
first addresses the saved exact input and its original completed output; it
does not recompute that historical identity with current defaults. Authenticate
its original policy/model envelope as untrusted historical construction data,
then rerun current proposal/source/staging validation before any effect. This
does not import an old positive witness or adjudication decision. A new native
conversation uses the current explicit envelope and the same remaining episode
budget.

The module must retain the raw reply and a private provenance file before the
fresh native scope returns. Provenance binds the role/version, workflow,
episode, original commit, exact request, actual provider/session and response
digest. Paired response-store keys are derived from the complete constructor
purpose in their own response/provenance domains, so they cannot alias witness
or ordinary scope entries. Indexed custody checks the actual admission,
resource charge, bounded counter and both immutable response entries. Physical
validation separately checks the private provenance file and its exact request
and reply binding before another paid dispatch. Neither check substitutes for
the other; a paired string in the database is not proof of a native session.

Existing authenticated source projections seed the first request. Further
source requests reuse controller-mediated immutable Git reads and the bounded
source-buffer implementation, with an explicit constructor caller interface;
they cannot fabricate a baseline source-selection observation. All prompts and
feedback remain bounded untrusted data. Pending admitted work with missing or
conflicting custody holds across role, provider and binding changes. Recovery
may restore the exact original output; it must not generate a replacement by
calling a different model.

The source interface now uses a controller-created `ConstructorTable` for the
episode's original commit. Its distinct table digest binds the literal commit
and ordered regular-blob metadata/paths; normalize Git column padding rather
than hashing display whitespace. Pages contain at most 128 entries and 32 KiB
of serialized data. Requests name that table digest and 1–16 ordinal selections,
with validated line/literal bounds. Model-provided paths, foreign table IDs,
symlinks and credential paths are rejected. The shared streaming source buffer
returns complete selected files when they fit, otherwise explicitly labeled
excerpts; required windows that do not fit fail without truncation. Final
serialized evidence must fit its controller-supplied residual budget. These are
data reads, never witness custody or evidence of behavioral red. The historical
witness-source ordinal protocol is unchanged.

`ConstructionEpisode` is a controller-only type with no model deserializer.
Its factory validates the original proposal against its registered contract,
the workflow's protected registered source/hash, and independently measured
original environment. Production derives the expected workflow acceptance
namespace independently. The private active workflow manifest must point to the
exact enrolled prepared artifact; its source, revision, registration hash,
original commit and pinned file data must agree. Recovery metadata alone is
not enrollment and cannot mint a replacement episode. The connected regression
rebinds/checkpoints a newly constructed replacement through the recovery API
and proves that it still cannot create a fresh budget without protected
enrollment. New dispatch rereads that enrollment as well as the workflow
binding. These checks authenticate enrollment data, not positive qualification
of an invalid old verifier. The existing engine emits
`sandbox-protected-cache-v2:<digest>`; unknown or malformed identities hold
rather than becoming a new budget. The factory ignores current candidate/main,
feedback and display-policy fields. A newly constructed but unactivated
registration cannot create another episode. Native requests and constructor
source tables expose typed episode factories; the raw base/episode constructors
are private. Native dispatch rechecks that the retained episode still matches
the workflow's active registered context. Candidate/evidence binding changes
preserve its budget; a different registered context invalidates that grant.

The indexed-custody regression first reproduced a dispatch past a lost response
after restart. The implementation now checks the actual admission, charge,
counter and paired response entries before any fresh native admission, across
provider/role/binding changes. All 222 durable-workflow tests pass. These tests
script response-index data only; they do not establish physical provenance,
original behavioral red, witness qualification or a live constructor.

The next connected regression reproduced a fresh native admission with paired
database strings but no private proof file. The actual admission observer now
also validates constructor evidence in its independently derived controller
namespace: immutable request bytes, request/provenance digests, workflow,
provider/session ownership metadata, response digest, file ownership/mode/links,
and separation from worker storage. Focused constructor tests pass (5), as do
265 workflow tests (one existing ignored test). The explicit production
read-only source probe still authenticates all eight retained #2014 files,
reconstructs the bounded complete request, and verifies unchanged workflow
state with no model calls. The existing witness producer remains unchanged.
These are transport-custody results, not behavioral acceptance. Native transport
and its pre-release persistence now have an implementation and connected
scripted-native tests; the production pipeline remains disabled pending the
source/staging/held-work integrations.

The constructor tests cover completed replay across restart, failed initial
paired-index publication followed by original-output recovery, an admitted lost
output that remains held, and free setup refusals with zero admitted attempts.
The production adapters have CLI-capture tests for the exact sealed model and
payloads, plus guard tests rejecting option/payload changes before dispatch.
The broader provider suite passes (747, with 16 opt-in tests ignored); constructor,
source and episode tests pass (14), and workflow regressions pass (266, with one existing ignored
test). These counts do not claim the full CLI suite passes: the separate
source-only-before-authority regression remains RED until staging integration.
The legacy source slice also passes (14). The read-only production probe
authenticates the actual #2014 active enrollment and retained original source,
reconstructs the same eight files and bounded complete request, and proves
unchanged workflow state with zero model calls. Fresh read-only review rechecked
the recovery-metadata finding after the protected-enrollment fix and found that
specific boundary resolved; full handoff readiness remains unproven.

The constructor transport saves `constructor-input-<exact-input>.json` before
admission. After a completed native response it privately retains the raw text
at `constructor-response-<response-digest>.txt`, the content-addressed native
provenance artifact, and an immutable
`constructor-completed-<exact-input>.json` reference. It then writes the existing
paired response index before leaving the fresh native scope. Every file remains
within the protected workflow namespace and is checked before further dispatch.

On restart the controller first authenticates the exact original admission
against its charge, bounded counter, provider and original binding. If the
private completed output exists but the paired-index transaction never
committed, restore that same original pair through the existing immutable
response-store API. Do not charge another call or reconstruct text with another
model. A recorded-but-missing/corrupt index remains a hold under the existing
store's checks; this is recovery of an unpublished original response, not
permission to overwrite conflicting history. An admitted call without its
original completed output holds. An unadmitted setup refusal preserves its
original error, spends no call/attempt and may retry once prerequisites recover.
Completed-cache replay precedes any new native runtime/capacity admission;
ordinary owner/source/lease checks still apply.

The native transport, canonical episode factory and bounded source interface are
still unused components. The persistent proposal/source/measurement loop,
restricted held-work admission and staged-proposal integration are required
before enabling a production constructor call. Cross-episode recovery must use
an explicit data-only path for an old admitted request; it cannot relax the
new-dispatch episode check. Scripted native conversations in disposable tests
are not a production model run, behavioral red or independent approval.

## Open decisions — owner: implementation agent

These must be resolved against source and audited before handoff, not guessed by
a downstream implementer:

- Integrate the selected native/source interfaces in the persistent constructor
  loop. Keep current episode grants separate from data-only recovery of an old
  admitted input after an environment/registration transition. Generic scope
  calls cache native custody only with qualification context; do not relabel a
  constructor as a witness to borrow that cache.
- Select the restricted construction lease/admission integration. Inspection
  leases forbid models. Normal claims cannot merely clear an unchanged completed
  decline to run a constructor. Specify eligibility, fairness, termination and
  the evidence that ends the canonical hold.
- Make the proposed actual-call budget atomic across bindings and nested source
  calls. The accounting foundation now checks the controller purpose
  `verifier-construction-native-v1:<episode SHA256>:<exact-input SHA256>` in the
  same transaction as actual native attribution and the workflow charge. It
  counts at most one dispatch per exact input and six per episode across all
  bindings; a refused transaction spends neither. General `BoundedNativeCall`
  counters remain keyed by full binding. Producer integration must install this
  purpose for every construction source/format/proposal conversation, including
  nested calls; an outer task-local wrapper alone does not do so. The purpose
  parser does not establish that the producer computed the episode correctly,
  grant a lease, recover missing replies or prove physical response custody.
  Those remain explicit producer/admission integration requirements.
- Choose the staging-value representation and replay boundary so it cannot be
  supplied by untrusted JSON or mistaken for approval. Preserve existing pending
  amendment compatibility, including environment-transition registration changes.
- Complete the actual fixture-interface investigation for the remaining card
  routes and approval/send seams before treating a #2014 replacement as ready
  for its remaining authority admission. The generic construction service must
  validate all supplied original criteria, not special-case that issue.
