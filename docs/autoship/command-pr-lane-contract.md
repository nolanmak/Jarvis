# Queued PR execution during an active issue build

Owner: #1706, within #1703. Working contract; no implementation permission or
accepted independent code review is established by this document alone.

## Problem and scope

Main 416f392b polls command jobs only between complete `run_once` attempts.
PR1944 waited over two hours and PR1945 over 27 minutes while an issue builder
retained its lease. See 1703-active-build-queue-starvation-proof.json. The live
observation does not establish spare physical capacity or a hung builder.

D1. Add a bounded PR command consumer to the existing independent worker. Reuse
the existing resume lane, command table, admission path, workspace/target locks,
resource supervisor and workflow leases. No second supervisor/service, new
allocator, owner-state reset or cancellation of progressing work. This addresses
PR-versus-issue head-of-line blocking. It does not finish #1706's broader
within-lane fairness/inactivity contract or automatic host sizing.

D2. An early return from a fresh issue build is not an acceptable shortcut: the
current fresh issue entry repeats scope/setup. No yield may discard its local
continuation or reset durable family/episode budgets. This change leaves that
entry and the original active builder untouched.

## Selection and ownership

D3. Provide a PR-only scheduling operation in the existing WorkflowStore. It
uses the same IMMEDIATE selecting transaction, five-minute schedule, attempt
receipt, owner-pause/dependency/eligibility/stage/active-lease predicates and
normal-lane fairness turn as next_command_job. General callers keep their
current behavior. A PR candidate means a validated PR target in command_job,
or an original issue command whose existing workflow has an attached PR.
Unbound issue commands cannot occupy this consumer. A selection reserves a
scheduling turn only; it cannot authorize execution or establish resources.

D4. Both consumers share the same command_schedule/command_attempt fencing.
Concurrent connections cannot select the same due scheduling turn. The
existing automatic-versus-command yield_normal turn remains shared; a PR-only
consumer may clear that yield and return no work just as the general consumer
does. This is a scheduling hint, not evidence that an automatic run completed.
No separate fairness ledger or new schema/backfill is needed.

D5. PR execution enters existing work_once_mode in Background mode, with all
its authorization, lifecycle, exact-target, pause, pending-mutation and readiness
checks. Once target resolution establishes a PR, use the existing task-local
Resume lane. An issue remains Combined. Never change process environment while
futures run concurrently. Resume uses its existing separate controller and
lane lock. Existing per-PR worktree lock and workflow lease still exclude two
owners, including the ordinary automatic resume path and foreground CLI.
The same candidate branch cannot be checked out in two worktrees. An original-workflow lease, target ownership or worktree ownership conflict
must defer, never force ownership or migrate its binding. A changed source/target
identity is instead a typed authorization/invalid-target rejection; it is not
ownership contention or permission to replace the durable source. New lane selection grants no new trust/provenance authority.

## Worker lifetime and accounting

D6. The PR consumer runs concurrently with the existing build and unpaid merge
reconciliation futures, in the same deployed service/slice and original
supervisor lock. Poll every 30 seconds with skipped missed ticks. Resolve the
repository only when jobs exist, bound lookup to 30 seconds, and retry unknown
identity later. No due work means no provider invocation. After finishing one
attempt, poll again on the cadence rather than spinning through failures.
Native and full composed run resources remain checked/reserved by existing
execution; two lanes never imply that two full runs fit on this host. For example, two
full 6 GiB runs cannot fit a 10 GiB shared ceiling: the second must defer rather
than lower either declared envelope. Queue progress is not capacity restoration.

D7. Before selecting productive work, apply the existing merge dispatch
allowance check. Reserve/settle compute attempts in DailyCounter using the same
flocked update and actual completion day. Feed billed PR work into the existing
resume fairness accounting. No attempt, queue claim, retry, external/manual
merge or status receipt creates confirmed autonomous merge credit. Finish the
selected command attempt through the existing timestamp fence and precise
outcome mapping, preserving authorization rejection and owner pause priority.
Extract and share that mapping rather than maintain divergent command handlers.
Settle paid fairness before idle/held cadence decisions: paid deferred reports
remain paid. Derive PR resume classification from the established resolved PR
identity, including issue-backed and independently adopted PRs; do not trust
the default resumed=false on RunReport::built.

D8. On executable/configuration upgrade, stop starting new PR attempts and wait
for any original command future to finish before returning UpgradeRequested,
just as the worker already waits for active reconciliation. Both active build
and active PR attempt must finish their existing bounded operation/cleanup
boundaries; a reload is not cancellation permission. Shutdown retains existing
operator cancellation semantics and existing descendant/lease cleanup contracts.
No second startup or queue reset substitutes for observing the original handle.
A single job failure is recorded/logged and cannot terminate the whole worker.
The PR consumer must compare the same executable/configuration watch snapshot
captured for this worker, not capture a new configuration after a change and
thereby treat it as already loaded. Detect changes before new selection. An
upgrade detection stops new admissions even while an original build is active.

## Required TDD evidence

T1. Against the old selector behavior, a real disposable store with an older
unbound issue and a ready PR must demonstrate the missing PR-only selection.
The new selector picks the PR; the issue has no attempt receipt or schedule
change. General selection still serves the issue. Cover an issue already bound
to a PR, other repository, pause, future retry, active lease, delegated parent,
terminal state, milestone ceiling and simultaneous independent connections.
A test that only fails to compile because an API is missing is not behavioral RED.

T2. Exercise the actual production worker scheduling coordinator with fake
clock and scripted command/provider boundaries: keep a progressing issue
attempt live, enqueue an authorized PR, and prove its selection/execution
entry advances within two 30-second polls with explicit sufficient declared
capacity. Original build remains live/uncancelled. Observe the durable command
receipt, not just a callback count. No source-substring acceptance assertion.

T3. Exercise actual command resolution and task-local lane selection with real
temporary Git repositories/scripted GitHub. Two PR consumers/automatic resume
cannot own the same target or worktree; issue requests keep Combined. Unknown
resources, exhausted native retained storage or insufficient composed resources
produce the existing precise deferral and zero provider calls. Ordinary code
review, exact-head CI, provenance, owner pause and merge permission still gate
execution. A fake resource result is not a production-capacity proof.

T4. Upgrade during active issue and PR work waits for both original outcomes;
no new command starts after drain. Repeat with active reconciliation, shutdown,
a failed command, late result after reselection, day rollover and zero allowance.
Original reservations, candidate artifacts, immutable evidence and holds survive.

## Deferrals, risks and exit

Per-PR-operation inactivity, same-lane ready-peer fairness, original issue stage
continuations and automatic capacity remain #1706/#1917 work. Their carrying cost
is that a progressing PR attempt can still occupy the resume lane, and a full
host still refuses new launches. No additional schema/backfill is introduced
by this consumer. Any change needed to those guarantees requires an explicit
extension rather than claiming this bounded change completes them.

The concurrency risk is existing mutable global or shared candidate state that
was only safe under serialization. Before implementation handoff, source-check
lane controller paths, task-local propagation, native/compute reservation writes,
review history, worktree/lease acquisition before mutation and release/upgrade
ordering. A discovered unguarded writer blocks enabling concurrent consumers
until its existing authority is used or the writer is fixed and tested.

Exit: behavioral RED, implementation GREEN, affected suites/hosted exact-head
checks, independent code approval, normal merge/updater rollout, then queued PR
progress with effective resource/worker evidence. Contract audit alone satisfies
none of these implementation/live exits. Parent #1703 keeps all original CLI,
Discord, autonomous shipment, fault and soak acceptance requirements.

## Source-audit findings and implementation blockers

Fresh-reader and all nine mapped seams were checked against the contract and
actual main 416f392b. The author checked conversation scope/source evidence.
Four concrete gaps were confirmed; concurrent enablement remains blocked:

- Provenance/controller identity: FallbackReasoner::track_review_history uses
  canonical controller path, and review_history keys it with branch. Switching
  from controller-build to controller-resume changes the lookup key. Owner
  #1706/#1707 must settle stable repository identity plus conservative existing
  history compatibility and real controller-transition tests before enablement.
  Missing/incomplete history cannot be promoted into complete provenance. This
  is a load-bearing prerequisite; merely changing task-local lane is insufficient.
- Shared attempt history: AttemptHistory::save uses one .tmp path, and both
  record_attempt/record_reasoner_error perform unflocked load/push/save. Owner
  #1706 must use one original-path locked read/modify/write authority for every
  production writer, with real concurrent-process preservation/failure tests,
  before enabling the additional consumer. A unique temp name alone cannot
  prevent lost read/modify/write updates.
- Fairness classification/settlement: targeted execute_pr does not reliably set
  resumed=true, and run_builds matches idle before billed. #1946 / draft #1947
  starts the bounded paid-held settlement repair with production-worker tests.
  PR classification at the real targeted boundary remains required separately;
  synthetic report tests do not prove it. No concurrency activation is granted
  by merging a fairness helper.
- Source identity outcome: work_once_mode's changed-source ensure is untyped,
  becoming generic execution-failed in the current mapper. D5 now distinguishes
  source/target mismatch (typed invalid-target rejection) from ownership
  contention (defer). Owner #1706 must cover the actual resolution/receipt path
  before sharing the mapper with the new consumer.

Six seam groups held; S3 and S6 exposed the provenance/fairness mismatches.
S7 exposed the ambiguous source-conflict outcome, now clarified in D5 with its
actual producer still a tracked implementation gap. Design findings are not
independent code approval. All remaining main/live exits above are unchanged.
