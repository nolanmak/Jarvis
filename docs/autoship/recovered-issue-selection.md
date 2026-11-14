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
- Reopens protected state after filesystem inspection and requires the original
  snapshot still to match before skipping. A genuine recovery during inspection
  cannot stay hidden behind an older held snapshot. Disappeared or unreadable
  history is unknown, never a fresh-work authorization.

Missing, malformed or unowned per-issue history preserves the hold and allows
inspection of peers. Unavailability of the shared owner-control store itself
retains the existing global fail-closed behavior. If no eligible peer exists, the existing fixed unavailable-history
outcome is reported. Selection never registers, claims, refreshes native capacity,
writes workflow events, changes owner controls, resets counters or publishes a
command job. SQLite borrows/transactions do not cross async filesystem reads;
only owned immutable inputs cross those awaits. The final productive claim
remains the atomic authority and rechecks concurrent pause, lifecycle and input
changes. A filesystem change after a free preview can delay selection until a
later poll; the preview is not a reservation or admission grant.

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
changed owner/candidate/compiler input, upstream-only movement, paused/untrusted/
manual/research sources, missing context and a recovery arriving during inspection.
Snapshots, events, model calls and command-job absence remain unchanged during
selection. Temporary database fault injection is confined to disposable fixtures.

The explicit ignored live diagnostic was run read-only before opening the repair
PR. It confirmed that actual #1830 is present in the existing recovery-priority
view and actual #1833 is a provably unchanged owned input hold, with identical
before/after snapshots, events and historical model calls (1.28 seconds). It
performs no claim, source edit, provider call or dispatch. It is a selection-input
check, not proof that this patch is deployed or that ordinary work has shipped.

Normal exact-head CI, independent engineering review, supported rollback snapshots,
protected merge and updater rollout precede live acceptance. Observe ordinary
work naturally after rollout, without resubmission, source edits, SQL writes or
counter resets. Repair PRs are manual engineering and count zero autonomous merges.
This selection fix does not establish ordinary PR/merge completion or close the
remaining #1703 capacity, custody, CLI/private Discord, five-merge or fault/soak
acceptance requirements. Separate #2058 correction construction remains unfinished.
