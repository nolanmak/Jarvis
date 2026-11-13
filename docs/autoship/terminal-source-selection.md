# Completed-work selection (#2076)

Status: second engineering-review amendment validated locally: 2,061 CLI
tests pass, with zero failures and 41 existing opt-in ignores (320.63s).
All 38 affected spec/source seams have been rechecked. New exact-head hosted CI,
independent engineering review and rollout proof remain required. PR #2077 is not
deployed.
Part of #1703/#1706. Normal engineering repair, excluded from ordinary autoship.

## Problem and evidence

The worker repeatedly selected open issue #1887 on November 13, although its
canonical workflow records PR #1888 as merged on November 6. Supported status
reports stage `merged`, action `complete`, no active lease and 26 unchanged
historical model calls. GitHub confirms the PR is merged; the issue remains open
without a close/reopen event. Main contains the reported fix. These selections
are neither productive model work nor additional autonomous merges.

The picker already excludes paused sources, but did not inspect terminal
workflow state before returning an open issue. The execution claim correctly
refuses terminal work. That preserves accounting while wasting selection turns
and preventing the no-candidate retriage path from running on those turns.

## Decisions and reasons

1. Reuse `FreshIssueControls` and the existing read-only `WorkflowStore` authority.
   Read the canonical issue workflow snapshot and skip `Stage::Merged` and
   `Stage::Retired`. A separate completed-issue cache would duplicate authority.
   Canonical issue identity is the existing lowercased trusted repository plus
   issue number. Do not infer issue completion from an arbitrary PR association;
   the existing PR alias/claim and linked-PR authorities remain unchanged.
2. Apply the filter in both automatic fresh-issue selectors after owner-pause
   checking and before returning a candidate or performing refusal mutations.
   Preserve each lane's existing trust and explicit-approval policy, ordering,
   pagination, manual-routing, daily history, linked-PR and readiness gates.
   The shared automatic gave-up sweep also excludes observed terminal/paused
   sources before reserving its existing cursor. After authenticated timeline
   and current-issue rereads, reopen the protected controls and recheck before
   writing a pending receipt or removing a label. This covers both the picker's
   no-candidate handoff and the independent periodic sweep. Unavailable or malformed
   protected history must never become permission to mutate a source.
3. An absent workflow row or absent state file does not make a new issue
   terminal. Selection must not create either. The selection-control reader
   opens the existing store read-only and probes its existing snapshot and
   owner-control lookup queries with an empty, non-workflow identity. This
   checks the required query capabilities without reading a candidate, creating
   tables/rows or migrating state; it retains the owner reader's existing
   absent-owner-table compatibility. This is not full schema-version or row-data
   certification. An incompatible required query fails when opening selection
   controls even for an empty inventory. Store opening/query-capability failures
   retain the typed `OwnerControlUnavailable` hold; initial eligibility pause-check
   failures retain their behavior. A terminal-history read failure for one candidate instead
   holds only that candidate: report a public-safe unavailability reason, preserve its record/labels/charges, and continue to healthy peers.
   Never infer nonterminal state from that failure. If no eligible candidate
   remains after such failures, return the retained typed error, not healthy
   idle. Both selectors use this rule. The shared readmission sweep likewise
   excludes unreadable candidates from reservation, processes eligible peers,
   then returns the retained error as a degraded result; successful peer
   receipts remain committed when reporting that degradation. At the final mutation
   boundary, a failed candidate-specific pause read also retains the typed error,
   preserves that source and processes eligible peers before reporting degradation.
   A successful paused result still preserves its source without a mutation.
   The error type is retained for caller compatibility, not a new owner pause.
   On no-candidate failure the primary caller projects `RunReport::held`; the
   legacy configured caller retains its existing error report. This repair
   introduces neither a uniform legacy workflow/reporting service nor a second
   durable hold ledger. The per-call retained error is only reporting state.
4. This read is a fairness filter, not admission. In the primary guarded lane,
   a terminal transition after selection remains blocked by the existing
   productive claim. The configured legacy execution lane has no such durable
   claim; its actual source/owner/model-admission integration remains #2056.
   A primary guard fixture after configured selection proves no configured
   admission protection. That existing gap is owned by the implementation agent,
   must be resolved before full #1703 all-ingress acceptance, and does not block
   these free filters from landing independently (as already scoped in #2056).
   Neither this reader nor a new issue-body edit may reset or reopen terminal
   history. No new terminal-reactivation policy is introduced.
5. Selection performs no remote write, workflow registration, model call,
   worktree creation, counter change or owner-control change for work observed
   terminal at the relevant free check or mutation boundary.
   It reports that the workflow is already merged/retired, then considers peers.
   If no candidate remains, preserve existing idle/retriage behavior for the
   remaining nonterminal, unpaused sources. A periodic sweep cannot bypass the
   same eligibility rule.

GitHub supplies no label compare-and-swap. Existing timeline/current-issue
rereads plus the final local protected-history check narrow, but do not eliminate,
the race immediately before a remote label request. This accepted external race
grants no workflow lease, model call or new shipment credit. Primary claims still
reject terminal/paused work; configured claim-time protection remains #2056.
Existing provenance and receipt states are retained recovery evidence only.
`pending` means an uncertain remote outcome; `readmitted` records completion
publication, not a guarantee that the label cannot later change. No production
reader/reconciler for pending receipts currently exists. #2078 owns that existing
recovery gap: implementation agent, after urgent #2077 rollout and before full
#1703 uncertain-effects/fault-soak acceptance. Until then retain the exact
receipts and evidence, inspect GitHub state read-only when needed, and never
claim automatic settlement or erase/replay pending work to make health green.
This repair adds no reconciler, distributed transaction or parallel ledger.

Here, a *terminal workflow* is exactly an existing canonical issue checkpoint in
`Merged` or `Retired`. An *open issue* is GitHub's remote lifecycle state and can
outlive that workflow. A *selection* is a free candidate choice; a *claim* is
separate guarded execution ownership. None is a new shipment receipt.

## Non-goals and remaining obligations

Production state changes use supported commands; SQL corruption/concurrency
injection occurs only in disposable test databases. No production SQL edits.
No automatic issue closure, workflow re-enrollment, author allowlist change,
bot approval, owner-pause clearing, new ledger, new schema or budget reset.
There is no new label-removal authority: preserve existing authorized nonterminal
readmission, while preserving labels on blocked sources. Explicit owner commands retain their current authorities. Manual repair
merges count zero autonomous shipments. No repair PR is an autoship test case.
This fix does not complete #2058, host sizing/C0-C5, review custody, Discord
coding/progress or the full ordinary end-to-end/fault/soak acceptance in #1703.

## Test-first exit and rollout

Four real-selector regressions failed against main before implementation:
terminal A/B starving ordinary C, all-terminal inventory failing to idle,
malformed checkpoint authorizing selection, and terminal-after-selection claim
coverage (whose initial selection was already wrong).

The fixtures run both primary and configured-repository selectors against
scripted GitHub inventories and disposable protected history. They preserve
snapshots/events/calls and owner controls, use case-normalized repository IDs,
forbid mutating GitHub calls and model executables, and assert no checkout or
implicit peer registration. Existing selection tests cover absent state,
unreadable state, trust, pauses, manual routing, complete pagination and owner
resume. The primary-only race test changes a disposable peer to retired after selection
and checks that its actual productive acquisition returns no guard without
changing history. Configured selector fixtures establish filtering only; #2056
owns its actual continuation/admission test. Valid automatic-label-proof fixtures
must cover terminal history, late terminal arrival, malformed protected history
and owner pause through the real shared sweep, with no label or pending-receipt
write for the blocked source. Mixed corrupt-history/healthy-peer cases exercise
both real selectors and the shared sweep: the healthy peer advances while the
bad record and its receipts remain untouched. Exhausted unknown-only inventories
retain the typed degraded outcome. Existing valid nonterminal readmission must
still work.

Run the selection group and shared-readmission fixtures, then the full managed
CLI suite. Before rollout obtain exact-head hosted CI and independent engineering
review. Declare both snapshot scopes from the live daemon/worker configuration:
application DB from the effective global `--db`, `AUGMENTAGENT_DB`, or `data.db`
relative to that process's working directory; workflow DB from
`AUGMENTAGENT_AUTOPR_WORKFLOW_DB` or its normal state-directory default. If these
resolve to distinct databases, take two supported snapshots with explicit global
`--db` arguments and distinct labels. Verify each manifest's `source_db` equals
the declared canonical database path. Run snapshot commands from the canonical
deployment checkout that owns the live daemon/worker release, never the task
worktree: binary capture uses that cwd's `target/release/augmentagent`. First
observe both live executable hashes and the deployed release hash; wait for a
coherent pre-rollout build if they differ. Require `has_binary: true` and the
saved binary's hash to match that observed pre-rollout executable identity.
A database-only snapshot or wrong binary leaves rollout pending. Snapshot allocation/refusal and retention follow `AGENTS.md` and
`docs/disk-storage.md`; no raw copy or limit/pin override is a substitute.

Update the branch with a normal merge from main if needed (never rebase or
force-push), merge normally and verify the updater's actual daemon and worker
builds. Then observe a natural cycle that actually encounters an existing
terminal source: require its explicit skip plus a peer choice or exhausted
inventory, and confirm the skipped workflow's protected history/charges did not
change. Ordinary selection or idle without a terminal encounter does not exercise
this repair; keep behavioral proof pending in that case. Never synthesize an
ordinary issue, change its labels or submit a repair as a test. Remaining live
proof is owned by the implementation agent and blocks calling this repair
deployed or #1703 restored. Full #1703 ordinary issue-to-merge acceptance remains
separate even after this free selection behavior is demonstrated.


## Local checkpoint before engineering review

The final managed CLI run passes 2,056 tests, zero failures and 41 existing
opt-in ignores (318.02 seconds). The selected selector group passed 19 checks;
the shared readmission group passed seven, including four new valid-proof
regressions that failed before its mutation-boundary fix. All 49 mapped spec
seams were checked read-only, with resolved/rechecked findings for sweep
eligibility, primary-only claim coverage, snapshot scope and live proof. This
spec/source audit is not independent engineering approval or deployed evidence.


## Engineering-review amendment

CodeRabbit's exact-head review of `8e09b2bc` identified that propagating a single
candidate's terminal-history error stops later eligible issues. The same
pattern affected the primary selector and shared readmission sweep. Two mixed
corrupt/healthy regressions failed before the source-local failure fix. The
contract above now distinguishes candidate-local unknown state from shared-store
and initial pause-authority failures, and keeps exhausted unknown inventories
explicitly degraded. Further test-first boundary checks reproduce late protected-
history corruption and an incompatible store with an empty inventory. The final
mutation check now retains degradation, and reader opening probes its actual
query capabilities without migration or registration.

At `79d26ab5`, the focused `terminal_` group passed 22 checks (2.87 seconds),
and the managed CLI passed 2,060 tests, zero failures and 41 existing opt-in
ignores (321.24 seconds). A spec/source audit checked 72 mapped seams and
rechecked eight affected seams after corrections within that earlier scope.
All hosted checks later passed on that head. The independent source review then
found the late owner-control reporting gap below, so neither those results nor
the earlier audit certify the next amendment. No deployment or autonomous
issue-to-merge acceptance is proved by these historical checkpoints.


## Late owner-control reporting amendment

The direct exact-head source review of `79d26ab5` found that the sweep's final
candidate-specific `paused` query still used `?` inside a per-item outcome whose
outer handler only logged errors. A malformed owner-control row arriving after
initial eligibility therefore preserved the label but could report `Ok(())`.
Retain that late typed error just like final terminal/open failures, continue
healthy peers and return the retained degradation. This changes final-boundary
reporting, not owner intent or the initial pause-query failure policy. The
regression corrupts only a disposable owner-control row during the current-issue
read, after both timeline proof reads, and asserts the typed result and healthy
peer completion. It also asserts preservation of the blocked workflow's snapshot,
events, charges, corrupt row, label and absence of a pending receipt. The new regression failed on the intended false-
success assertion before the fix (0.30 seconds). After the fix, the focused group
passes 23 tests (2.85 seconds), and the complete managed CLI passes 2,061
tests, zero failures and 41 existing opt-in ignores (320.63 seconds). All
38 affected seams from the existing 72-seam map were rechecked and hold; this is
source/spec inspection, not independent engineering approval. New exact-head
hosted CI/review and natural rollout verification remain separate requirements.
