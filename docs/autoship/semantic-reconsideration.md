# Authenticated semantic reconsideration (#2038)

This is the implementation contract for recovering an autoship workflow held on
a completed original-baseline `measurement: unknown` judgment. It supplements
[qualification recovery](qualification-recovery-contract.md). It does not replace
#1703's ordinary issue-to-autonomous-merge, daily shipping, host sizing, Discord,
custody or live fault/soak acceptance.

## 1. Decisions and vocabulary

D1. A fully authenticated Unknown is negative source data, eligible for the
existing independent verifier-definition repair to consider. It is not evidence
that the implementation or verifier is defective, a positive qualification, an
approval, or permission for a builder. The independent authority may decline.

D2. Missing/corrupt/unmatched/ambiguous source remains an unpaid evidence hold.
Do not ask the completed witness question again, remeasure to fill missing
history, fabricate changed owner input, reset counters, or promote an unactivated
contract. Existing amendment activation remains authoritative for replacements.

D3. A **cue** is the protected `original_semantic_witness_recovered` event, not
an execution lease. Its key hashes `(semantic-reconsideration-v1, workflow,
judgment identity, qualification policy)`. Generation and owner revision fence
transactions but do not renew that key. A **loss episode** is a separate
`semantic_source_unavailable` event for a current productive generation and
checkpoint; restoring it continues the same cue, not another reconsideration.

D4. A **prepared source** contains the complete proposal, registered original
baseline and receipts. A prepared source is not a qualified baseline. Existing
positive witness, original behavioral red, candidate green, scope, hosted CI,
independent review and merge receipt requirements all remain mandatory.

D5. Keep the existing per-input two and original-environment-epoch four repair
limits. Amendment `environment_id` uses runner/configuration/dependency and
executable inputs; the separate Rust executor cache fingerprint is not that
epoch. Controller/classifier edits and workflow generations must not create
fresh repair budgets. Actual environment transitions retain the existing
independent transition protocol. Preserve requests, outcomes, charges and native
sessions, including uncertain effects; do not directly edit production SQLite.

## 2. Source production and read-only authentication

The workflow's normalized repository and resolved durable ID select its private
acceptance namespace. PR aliases resolve to that same ID; an issue-linked PR
keeps the issue's contract source. Controller files remain outside worker roots.

Read an existing private prepared manifest/file or its unique original-baseline
orphan. When prepared data is absent, read the protected original Plan and
RecordedResult through the existing evidence index's digest checks. Require the
same workflow, input binding, source, base, original environment, complete
proposal/registration, original command admission and matching quiescence. The
shared executor validator additionally checks original capture/output when a
result reuses a command from an earlier attempt. The old executor fingerprint
identifies data; it grants no replay under current execution policy. An existing
manifest still constrains the active source revision/hash/base. Different
matching source identities are ambiguous; never choose by timestamp.

Verify registration metadata and its complete pinned source map. Missing
candidate adapters may be inspected as source data; changed/symlinked existing
adapters are rejected. Inspection installs no files, creates no manifest or
baseline, starts no command/model/runtime, and changes no attempts or charges.
The positive `verify_integrity` and observation gates remain strict.

Read all retained baseline-native admissions and their paired response/proof.
Require original charged custody, private immutable proof bytes, input/provider,
witness purpose/base/context, supported producer policy, complete judgment
schema/coverage and the exact full request hash reconstructed from proposal and
baseline. A historical context is negative data only. The proof must belong to
this workflow's native namespace. Raw diagnostics stay private; public status
exposes the blocker, not response text or private file contents.

## 3. Free inspection, atomic cue and normal wake

Reuse the existing qualification-recovery controller: four concurrent slots,
128-row scan, five-minute selection cadence/cooldown and 45-minute inspection
bound. Selection records fairness before remote lookup and creates no execution
claim. Recheck repository identity/write grant, author, source lifecycle and
body, PR dependency, retained checkout/branch/head and exact context binding.
The authenticated frontend request must still match the retained context. A
retained unpublished head needs the existing ownership/ancestor checks.

Inside one Immediate transaction compare the exact held checkpoint, binding,
generation, branch/PR, eligibility, idle lease, current owner pause/revision,
pending owner input and unresolved publication/merge effects. Authenticate source
using a borrowed read-only evidence reader in this transaction. If eligible and
its key is unused, atomically retain the cue and queue
`reconcile / original_semantic_witness_recovered / reconsider_verifier`.
Do not create a lease, increment attempts, charge models or change budgets.

The existing due/wake/adoption/issue selectors dispatch the hint through normal
selection and claims. A subsequent productive claim makes its usual generation
and attempt. Owner/source/dependency/effect and resource checks remain normal
requirements, not inherited approval from inspection.

## 4. Guarded preparation and launch boundary

The guard authenticates the stored cue header, key and event generation, and
uses it only for its exact current binding. Intermediate reasons such as
`resume_started` do not erase the cue or registered execution context. A recovered
checkpoint without its source reference fails before a new claim. Enter with
RecoveryOnly authority; this cannot authorize a builder.

The shared audit dispatch observer authenticates the required source before
preparation or launch. `run_resources::check_requirement` runs it before native
VM preparation in both native providers, and the process supervisor runs it
again before final model release. This covers readiness calls made before the
main preparation function. Do not replace this with a convention at one caller.

Normal preparation uses that same authenticated prepared source instead of
scoping/measuring it again. Only this ordinary guarded phase persists the
prepared file/manifest and installs immutable candidate adapters. Preserve the
original base and check every required criterion and candidate ancestry under
the existing preparation rules.

For unqualified work, qualification records RecoveryOnly registration metadata
and the actual controller-installed inputs before a native call or hold. Capture
the actual head again after preserving candidate edits, before repair can return
with an exhausted budget. This prevents controller-created dirt/commits from
making an unchanged hold look like new input on restart. Existing current
positive qualification retains its fast path.

Use the authenticated negative witness as `BaselineNotRed` repair feedback;
never persist it as current positive approval. Enter the existing bounded
independent amendment path. Its actual `verifier_definition_disputed` checkpoint
binds `amendment-input` before the independent authority call. A binding change
ends the old cue's applicability; a later correction stays in the existing
amendment/activation protocol. No accepted revision uses the existing bounded
repair-unavailable/exhausted hold, with the decision/diagnosis retained privately.

## 5. Source loss, wrappers and restoration

If a current cue's required source is unavailable at preparation/dispatch, atomically
record its restrictive Scope evidence-unknown checkpoint and loss reference under
the current lease and exact binding/key. Duplicate loss in the same generation
and checkpoint records no second episode. Admit no model or VM launch. Once held,
this attempt cannot self-resume even if the data returns immediately.

Generic failure checkpoints and physical/raw recovery reclassification cannot
overwrite this hold. Terminal controls retain their existing authorization and
uncertain-effect requirements. A real new owner/input binding follows normal
registration; it does not erase historical charges or create a new old-cue key.

Preparation wrappers report the already-held durable state instead of calling
the initial-enrollment writer on registered work. That writer's registered-input
fence stays strict. Accounting checks authoritative held state even when a
provider wrapper replaces the local error type. Previously paid admissions
remain billed independently of being held. Source absence is not host capacity.

After lease release, the same free controller validates the restored source,
owner/revision, pending input/effects and exact checkpoint/generation. Require
the original judgment identity, including when the current inspection policy
has changed. Atomically queue normal reconsideration and record
`original_semantic_source_restored` with the loss event's sequence. Keep the
original cue's key/policy and consumed budget. Consume each loss sequence once.

Repeated real loss in a later admitted attempt may restore that episode again.
An old generation/checkpoint or already-consumed loss sequence cannot revive a
later independent rejection or exhausted hold. Missing/wrong source stays held;
failed storage transactions roll back checkpoint and reference together.

## 6. Validation, rollout and open work

Build order: authenticated classifier/source reader -> atomic cue/deduplication
-> normal wake/guard context -> preparation/dispatch and loss continuation ->
exact-head hosted checks/review -> supported snapshot/normal engineering merge
-> updater proof for daemon and worker -> actual ordinary workflow outcome.

Actual failing regressions preceded classifier, selection, source reader,
normal consumer, restart and restoration fixes. Component coverage includes raw
source with no manifest/candidate harness; corrupted plan/result/admission/Q;
namespace/source/binding mismatch; lease/pause/revision/inbox/uncertain effects;
partial storage rollback; no new model on source loss; same-episode restoration;
wrapper accounting; exhausted-budget and controller-commit restart fencing.
The explicit read-only #2014 probe checks both original responses and unchanged
workflow/events. These checks do not prove an autonomous merge.

Current local validation: full CLI 1,965 pass / zero failures / 36 explicit
ignored tests; the actual source probe separately passes. Required Linux,
privacy and node-release checks and independent repair review must pass on the
actual final PR head; macOS is informational under existing policy. Take only
`augmentagent deploy snapshot`, merge normally without rebase/force-push, and let
the updater deploy. Verify both process executable hashes and selected policy.
Repair PRs are manual engineering, never autoship test cases.

Open work owned by this repair: audit every producer/consumer seam; finish
required exact-head CI/review, snapshot/main rollout, and observe ordinary
#2014 leaving its retained hold through this path. A read-only probe, a manual
repair merge, or an exhausted hold alone is not restoration. Keep #1703 active
until its complete ordinary shipping, five-confirmed-merges-per-UTC-day,
CLI/private Discord, host sizing, custody and unattended/fault acceptance is
independently proven. No synthetic benchmark is started by this repair.
