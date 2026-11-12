# Detached retained checkout recovery (#2046)

This amendment closes the normal resume producer to free qualification recovery
handoff in [semantic reconsideration](semantic-reconsideration.md). #2045 is
already deployed at main 173295c8; ordinary #2014's first inspection still held
because the consumer required a symbolic branch. #2038 remains open for its
actual ordinary outcome, and #1703 retains its complete acceptance scope.

## Decisions

D1. The workflow branch identifies the authorized remote PR branch. It does not
require symbolic HEAD in every candidate checkout. The normal
`prepare_resume_worktree_at_head` producer creates a detached linked worktree
because the published branch may remain attached to the original issue checkout.
Preserve that layout; inspection must not checkout, attach, reset, commit or
rewrite any candidate or ref.

D2. Accept either the exact expected attached branch, or detached HEAD at the
canonical resume path for the same PR. The latter is an existing producer shape,
not general permission for a detached Git directory. A shared read-only
`resume_worktree_path(root, pr)` derives the path from the verified primary root
and positive PR number; both producer and consumer use it. Validate every path
component with the existing checkout-path guard before canonical comparison,
including symlinks and dangling links. No arbitrary detached or issue-only
detached checkout is authorized by this exception.

D3. Both shapes retain complete original context and current input binding,
registered linked-worktree and common-repository checks, fresh remote identity,
write grant, trusted author, open source, issue/PR association, remote branch and
dependency checks. The context remains protected and digest checked. The workflow's recorded branch and PR, and the context's checkout, must
correspond to the refreshed target. A matching published
head is usable source data. An unpublished head still requires the existing
read-only `retained_candidate_head` authority, latest confirmed original push
receipt, exact retained head and Git ancestor proof. Neither source shape grants
publication, approval or merge authority.

D4. Reuse the existing unpaid inspector, lane lock, semantic transaction, normal
wake and productive claim. Owner pause/revision, pending inbox, lease,
publication/merge uncertainty, exact snapshot and original negative-source
validation remain mandatory. Free reservation and the atomic recovery transaction
must use the same complete merge-reconciliation predicate as normal dispatch;
a receipted operation with a confirmed dispatch ledger and matching nonterminal
checkpoint still needs merge reconciliation. Share that existing predicate rather than copying a narrower query. Nothing resets or clones budgets, charges or
sessions. Wrong or unavailable evidence holds without a model or candidate
mutation. No new registry, allocator, command or queue is added.

## Producer and consumer order

The resume producer checks the PR and branch, uses the shared checked path,
validates any existing linked checkout, fetches the requested branch and verifies
the resolved head. A new checkout uses `worktree add --detach`. Existing retained
unpublished work still requires the original ownership and ancestor proof.

Free recovery refreshes source authorization and the exact frontend request,
then takes the existing per-source lane lock. `validate_recovery_checkout` is the
same read-only consumer used by `recover`: it validates the linked checkout,
reads a successful symbolic-full-name HEAD result, accepts the expected full
attached ref or the matching PR's checked canonical detached resume path, then
recomputes the complete original binding. It also verifies unpublished ownership
and ancestry. Git failure, wrong attached ref or invalid detached path fails;
there is no fallback from a failed Git command to presumed detachment.

Only after that validation may the existing source inspector and Immediate
transaction authenticate the negative judgment and queue the one-use cue. That
transaction rechecks original snapshot/generation, owner control, idle lease,
inbox and unresolved effects. The normal guard repeats its source/input checks
before preparation and launch. No checkout exception substitutes for those
independent authorities.

## Testable build order and remaining gates

1. Drive the real resume producer into the actual recovery checkout consumer in
   a disposable committed repository. Record the specific detached failure;
   setup/compiler failures are not RED evidence. Preserve checkout/ref bytes,
   workflow snapshot/events and model accounting before and after inspection.
2. Verify published and owned unpublished detached candidates, plus existing
   attached candidates. Reject wrong attached branches, arbitrary detached
   worktrees, canonical path aliases/symlinks, foreign repositories, changed
   bindings/heads, missing or mismatched PR identity, unowned unpublished heads
   and non-descendant retained heads. Existing ownership and semantic transaction
   suites continue to cover pause, lease, inbox and uncertain-effect fences.
3. Run the explicit read-only actual-#2014 checkout compatibility probe against
   freshly read public PR metadata and the protected original context. It uses
   the same consumer and never executes the recovery transaction or a model.
   This proves source shape compatibility, not automatic recovery or a merge.
4. Complete relevant regressions, full CLI and bounded amendment seam audit;
   then required exact-head hosted CI and independent engineering review. Exclude
   the repair from autoship execution. Successfully take the supported deployment
   snapshot before the protected normal merge; then let the ordinary updater
   deploy and verify both service executable hashes/selected policy. If snapshot
   storage admission refuses, report it to the owner; do not bypass its limits. Manual repair merges count zero.
5. Observe ordinary #2014 naturally leaving its retained hold, independent
   verifier repair/qualification and the actual review/CI/merge outcome. Do not
   resubmit, alter owner input, edit the candidate or production SQLite to buy
   progress. No synthetic benchmark is started.

Current local evidence: the real producer/consumer regression and explicit
read-only #2014 checkout probe first failed at the symbolic-branch lookup, then
passed after recognizing the exact detached producer shape. The 17-case matrix
then exposed two canonical-path alias cases; moving the original component
checks into the shared path helper corrected both. All four checkout-recovery
tests pass (one explicit production probe ignored by default). Before the final
merge-fence correction, the full CLI passed 1,978 / zero failures / 39 explicit
ignores, and the fresh-authorization composed #2014 probe passed in 2.18 seconds.

Fresh eyes found no spec gaps. All 37 seams were checked; two findings were
resolved and rechecked, so all 37 now hold. Snapshot ordering is corrected above.
The merge-fence regression reproduced four initial/restoration selection/cue
mismatches; sharing the existing complete predicate corrects them. All 14
recovery-store tests pass, including unchanged allowance and normal terminal
reconciliation. Setup errors were excluded from behavioral RED evidence.

Final source validation at c9996266: full CLI 1,979 pass / zero failures / 39
explicit ignores (278.95 seconds). The composed fresh-authorization #2014 probe
passes (2.55 seconds); original negative-source authentication separately passes
with both retained responses (0.04 seconds). Both probes preserve workflow/events
and make no model call or candidate/ref rewrite.

Exact PR-head hosted CI/review, supported snapshot, rollout and ordinary outcome
remain open. These are source compatibility and component results; an inspection
pass or another bounded hold is not #1703 restoration.
