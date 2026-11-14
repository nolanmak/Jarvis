# Recovered and eligible issue selection (#2086)

After #2085 deployed, the normal inspector recovered ordinary #1830 at
2026-11-14 13:01:19 UTC, preserving generation 1 and twelve historical model
calls. The queue correctly woke the normal worker, but its newest-human-first
picker repeatedly returned #1833. That workflow remained at an unchanged
`readiness_assessment_invalid / await_readiness_input_change` hold, generation 7
and seventeen historical calls. Neither source made new model calls: selection
was busy while productive work stayed blocked.

The fixes belong at two existing boundaries. They create no alternate dispatcher,
model admission, qualification or merge authority.

## Candidate order

After the existing REST filters remove PR rows, gave-up/sliced issues and manual
engineering, stable ordering is:

1. Human issues with a due qualification-recovery transition.
2. Other human issues.
3. Research issues with a due qualification-recovery transition.
4. Other research issues.

Priority comes from the existing `qualification_reconcile::issue_retries`
reader, using `WorkflowStore::due_qualification_recovery` and keeping issue
workflows without a PR. It reads the **persisted post-recovery checkpoint**, not
the inspector's due-for-inspection reservation. The actual inspector transaction
sets `stage=reconcile`, `reason=original_semantic_witness_recovered` and
`action=requalify_original_measurement` for the motivating retained-witness case;
that transition creates the picker hint rather than consuming it. The same view
already recognizes the existing native-custody and original-measurement recovery
shapes. It requires a due, unleased, unpaused workflow with no pending owner
messages, publication operations or uncertain merge reconciliation.

A successful empty view differs from a failed shared recovery query. Failure to
execute that protected query stops fresh selection with the fixed
`qualification-recovery-history-unavailable` hold; it is not silently converted
into empty hints, spends no model calls, resets no ledger and cannot take the
per-issue-preview waiting-work fallback. Basic owner-control queries succeeding
does not prove the additional shared recovery query readable.

Individual malformed checkpoints are different: the due reader decodes the
canonical Checkpoint rather than letting one invalid JSON/schema row abort the
shared lookup. Such a row supplies no recovery hint and remains unknown to the
ordinary per-issue history/claim readers, which preserve it and inspect peers.
After a complete due query, an identified workflow's snapshot JSON/typed-column
conversion failure likewise supplies no hint; SQL/schema/storage failures still
propagate. Nothing infers terminal state, repairs the row or makes it fresh work.
Healthy recovered peers retain their priority over newer eligible fresh issues.

A genuinely absent store at the initial read still gives the existing empty
first-install result. Once the reader has observed due recovery history, losing
that store across repository inspection is a global unavailable-history error,
not a new installation or an empty successful view.

Priority lasts only while that current checkpoint satisfies the view. An active
lease, future eligibility time, pause/pending mutation or move out of the
recognized reconcile shape removes it. Releasing a claim still at that shape can
make it due again; this resets no charge or approval. Historical recovery events
alone never keep priority. The hint is protected scheduling data, not fresh
native-evidence authentication or permission to execute.

Newest-first order within each tier is preserved. Research still cannot consume
the human tier. Existing owner commands keep their separate queue priority.
Recovery still exempts the existing daily-refusal ledger, without resetting it.
A recovery hint is ordering data only: owner pause, terminal history, author
trust, blast-radius triage, linked PR deduplication, readiness, productive claim,
finite budgets and every later verification/review/CI/merge gate remain in force.

## Read-only unchanged-hold preview

Before returning a fresh issue, `FreshIssueControls` inspects only an existing
issue workflow in the shared `awaits_initial_input` shape. A recovered transition
in `reconcile` is not that shape and rejoins ordinary selection. The preview:

- Authenticates the existing protected execution context and exact serialized
  issue number/title/body. A genuine request change reaches normal acquisition.
- Requires the existing registered owned checkout, expected execution-lane path
  and issue branch. It never creates, refreshes, repairs or deletes a checkout.
- Uses the same retained-context compatibility and candidate-binding helpers as
  productive acquisition. Existing registered-context rules and the known
  initial-environment normalization are preserved. Upstream-only movement does
  not replenish attempts; relevant compiler/lock/policy or candidate changes
  still reach normal guarded acquisition.
- Compares the complete binding: head, contract and Git/environment/evidence
  identity, including current tracked/untracked candidate bytes and receipts.
- Checks the exact read-only native-custody recovery predicate also used inside
  productive claim. A physically held workflow whose original charged evidence
  has become readable again stays eligible even without a background inspector.
  Missing custody or a semantic hold remains blocked; this preview cannot grant
  a lease, replace the original evidence or admit another model call.
- Reopens protected state after filesystem inspection and requires the original
  snapshot still to match before skipping. A genuine recovery during inspection
  cannot stay hidden behind an older held snapshot. Disappeared or unreadable
  history is unknown, never a fresh-work authorization.

Missing, malformed or unowned per-issue history preserves the hold and allows
inspection of peers. Unavailability of the shared owner-control store itself
retains the existing global fail-closed behavior. If no eligible fresh peer exists,
the picker passes its unavailable-history error to the routing boundary below.
The fixed public hold is emitted only after any permitted waiting-work fallback
has found no work. Selection never registers, claims, refreshes native capacity,
writes workflow events, changes owner controls, resets counters or publishes a
command job. SQLite borrows/transactions do not cross async filesystem reads;
only owned immutable inputs cross those awaits. The final productive claim
remains the atomic authority and rechecks concurrent pause, lifecycle and input
changes. A filesystem change after a free preview can delay selection until a
later poll; the preview is not a reservation or admission grant.

## Waiting-work fallback after per-issue preview failures

An individual held issue's unavailable inputs must not suppress the combined
lane's existing waiting-work fallback. After the complete issue inventory has
been considered with no eligible fresh issue, a retained preview error carries
`IssuePreviewUnavailable` while preserving the original unavailable-history
cause. It is not a fresh issue or a cleared hold.

Only the existing reserved build slot (`prefer_build`) can use that result to
try the same waiting-draft and adopted-PR fallback already used for an empty
issue inventory. Eligible fresh/recovered issues still win that reserved slot.
Build-only and resume-only lane behavior remains unchanged. The extracted
production fallback still performs its existing draft/adoption, owner controls,
exact-head, readiness and productive claim checks; the routing helper grants no
execution or merge permission and does not itself reset fairness debt or resource
counters. Actual guarded fallback work retains its ordinary model/resource
accounting and scheduling settlement.

Before trying fallback after a preview error, protected shared history is
reopened and required to exist. If it became unreadable, structurally invalid or
disappeared, selection keeps the global unavailable-owner-history outcome; a
missing store cannot be reinterpreted as a fresh install in this continuation.
Incomplete inventory and global owner-control errors are never classified as
per-issue preview exhaustion and never take this fallback path.

If waiting work is available, its guarded result becomes the normal run result.
If none is available, the original preview error returns through the existing
fixed public unavailable-history hold. Empty inventory with no preview error
retains ordinary idle behavior after an empty fallback. The unchanged-hold
preview and its error-routing continuation do not resubmit, relabel or re-enroll
the held source or give it a new attempt. Existing normal blast-radius triage
still applies before preview, including to genuinely changed issue requests;
this repair does not suppress that pre-existing source classification.

## Verification and rollout limits

The regression invokes the actual issue picker in disposable child processes,
with a fake read-only GitHub API, real registered Git worktrees, protected holds
and a scripted typed recovery transition. The scripted callback tests selection;
it proves no original witness, native provider call or execution permission.
The fixture applies the actual `recover_retained_qualification` store transition
with scripted evidence validation, then calls the real picker after that
transaction has committed; it does not merely inject a reason string. The actual
production inspector transition was separately observed at 13:01:19 UTC. Before
the fix the picker chose held #1833 instead of recovered #1830.

Coverage includes recovered priority, eligible peers without a recovery cue,
a genuinely eligible newer peer, restored/missing/semantic native-custody holds,
changed owner/candidate/compiler input, upstream-only movement, paused/untrusted/
manual/research sources, missing context and a recovery arriving during inspection.
The real picker and production selection-routing helper also cover a preview
failure with waiting work, no available fallback, build-only behavior, malformed
global history, a store disappearing during inspection, and empty inventory with
or without waiting work. A scripted availability callback tests routing only;
it is not native execution or a completed draft-resumption receipt.
Snapshots, events, model calls and command-job absence remain unchanged during
selection. Temporary database fault injection is confined to disposable fixtures.

The explicit ignored live diagnostic was run read-only before opening the repair
PR. It confirmed that actual #1830 is present in the existing recovery-priority
view and actual #1833 is a provably unchanged owned input hold, with identical
before/after snapshots, events and historical model calls (1.28 seconds). It
performs no claim, source edit, provider call or dispatch. It is a selection-input
check, not proof that this patch is deployed or that ordinary work has shipped.

Normal exact-head CI, independent engineering review, supported rollback snapshots,
protected merge and updater rollout precede live acceptance. The implementation
agent owns that acceptance: verify both running binaries and effective worker
configuration, then correlate a natural selection/claim of already-recovered
ordinary #1830 (or a genuinely equivalent open issue) with its protected workflow
and retained original measurement. It must requalify that original evidence and
reach its next normal productive stage without a forced submission, replacement
witness or reset of historical charges. A selection log or heartbeat alone does
not satisfy that exit. The unchanged held peer must receive no new claim/charge
merely because its inputs stayed the same.

If suitable ordinary work never appears, or the observed workflow stays held
before retained requalification completes, live recovery acceptance remains
pending with the implementation agent; absence of evidence is not success. A
new independent blocker must be recorded and repaired through the normal path.
The waiting-fallback path has live acceptance only when a natural reserved-slot
opportunity correlates the per-issue preview failure with actual guarded waiting
work and preserves the original held source. Until then its scripted regression
is component evidence, and live fallback observation remains part of #1703's
fault/soak work rather than a claimed shipment. No production fault or source
change is fabricated to create such an opportunity.

 Observe ordinary
work naturally after rollout, without resubmission, source edits, SQL writes or
counter resets. Repair PRs are manual engineering and count zero autonomous merges.
This selection fix does not establish ordinary PR/merge completion or close the
remaining #1703 capacity, custody, CLI/private Discord, five-merge or fault/soak
acceptance requirements. Separate #2058 correction construction remains unfinished.


The later waiting-work review finding was reproduced through the actual picker
and production routing helper: an unavailable held-issue preview returned before
the scripted waiting-work callback (0.47 seconds, RED). The recovery-reader
failure-policy audit then reproduced a separate RED (0.64 seconds): basic owner
reads succeeded, the protected recovery query failed, and selection silently
continued as though no recovery hints existed. The final 23-scenario selector/
routing regression passes (12.53 seconds), preserving snapshots/events/charges
and command-job absence; availability callbacks remain scheduling data only.

The fresh-reader audit and all 30 consolidated seam checks (covering every one
of the 57 original mapped crossings and the added live-acceptance boundaries)
hold after resolving recovery-reader failure policy and the preview-versus-normal
triage scope. These read-only spec checks are not engineering review or rollout
acceptance. Final exact-head hosted CI/review and the natural live exits above
remain required.


The subsequent hosted CLI run exposed two unchanged terminal-history regression
tests: malformed individual metadata was incorrectly promoted to a shared-query
failure. Both were reproduced locally before the repair (three related tests
passed, two failed). The view now decodes canonical checkpoints and separates
identified-row conversion errors from SQL/schema/storage failure. Additional REDs
cover a valid-JSON checkpoint missing required fields (0.50 seconds) and an
observed store disappearing across repository inspection (0.45 seconds).

Current validation passes the complete affected self-improve suite (889 passed,
zero failed, 34 explicit ignores, 317.81 seconds), including those two unchanged
regressions and all 27 selector/reader/routing scenarios. The workflow-store suite
also passes (233 passed, zero failed). The current actual-input read-only probe
passes in 1.13 seconds with unchanged workflow snapshots/events/historical calls.
Changed seam checks were repeated against this source; no unresolved findings
remain. A full final-head hosted CLI/integration result, independent review,
normal rollout and the live acceptance exits above are still required. The local
long-scratch Unix-socket limitation remains separately tracked in #2088.
