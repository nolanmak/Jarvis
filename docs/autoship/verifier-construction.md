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

## Open decisions — owner: implementation agent

These must be resolved against source and audited before handoff, not guessed by
a downstream implementer:

- Select the existing producer interface that supports bounded source collection,
  protected per-input response recovery and a distinct untrusted construction
  role. Generic scope calls currently cache native custody only with qualification
  context; original custody permits source-selection and baseline-witness roles.
  Simply relabeling a constructor as a witness is invalid.
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
