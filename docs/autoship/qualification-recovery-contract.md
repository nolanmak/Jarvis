# Qualification recovery: source data and durable holds

Status: source replay and the unchanged productive-claim fence are implemented
locally for PR #2035; verified recovery and wake integration below remain a
load-bearing implementation task. This
is not a restoration, full #1952 closure, or permission to merge known defects.
Parent issues: #2034, #1952 and #1703.

## Decisions and boundaries

1. Source read selectors are data. A previous native source-selection response
   may supply revalidated immutable Git excerpts to a current independent
   qualification. It must never become a current-policy native judgment or
   final-review approval. This avoids reissuing an already completed original
   source-selection request without weakening current qualification policy.
2. The first compatibility recipe is pinned to main `7353e9cd`: scope policy
   `bd44467c5c2ddedd493a2c8ce392578669b5ec8570b622403d05d92625f20771`,
   qualification policy
   `7a16a5c6b515bdf9e065f955bcc1e436de04a692e29d764bd5f89b3979e1b998`.
   Rebuild the old qualification context from the exact proposal and measured
   baseline, then rebuild its original prompt. In the incident this reproduces
   both saved context and parent-request hashes, verified read-only against the
   original evidence. No arbitrary old policy is accepted.
3. A controller-owned `SourceReplay` record binds current epoch/request to the
   exact original input, provider, epoch, parent request, known policy and
   source-choice request hash. Creation verifies the complete original native
   input hash, original private provenance and retained admission custody.
   Consumption reads exactly that input, checks the source-selection role and
   every original field again, and revalidates selectors against current reads
   of the same immutable Git base. Original response/provenance rows stay
   unchanged. The record uses existing immutable scope-response storage; it
   does not copy a model response under a new native-response identity.
4. Current native witness replies and qualification artifacts retain current
   policy binding. Compatibility applies only to source selectors. Different
   workflows, original requests, roles, providers, source bases or unrecognized
   policies do not acquire compatibility through a shared parent epoch.
5. A typed qualification hold on enrolled work preserves its candidate binding,
   registration, original evidence, private diagnostics and expenditure. The
   initial-enrollment fence stays strict. Actual paid admissions determine
   billing through existing capacity accounting.
6. An in-memory builder refusal is insufficient. After guard release, unchanged
   productive claims for original-evidence and verifier-infrastructure recovery
   must be fenced. Adding only a permanent fence is also insufficient: verified
   recovery must reopen the same request through the existing unpaid
   reconciliation and claim authority, without resetting budgets or creating
   another work queue.

## Source-replay implementation and tests

The qualifier builds both request contexts from its immutable prepared record.
`prepare_source_recovery` publishes only a small reference projection. The
native scoper consumes it before generating source selectors; its eventual
judgment is still a fresh/current-policy response. A correction with a different
request does not silently inherit an incompatible projection.

Tests retain the direct-current-policy rejection and exercise the explicit
source-data path separately. Required negatives include another workflow,
another original request, another current request, a witness rather than source
role, unknown old policy, changed/missing provenance and mismatched native input.
Actual parent/context hashes from the incident were reproduced without writing
production state or copying the database.

## Durable recovery integration still required

Owner: the normal workflow controller. Trigger: before #2035 can merge.

- The existing productive-claim fence now covers the two recovery holds. A stale
  preview or a new owner message alone cannot replace missing original results.
- Define the protected original-recovery observation and its atomic consumption
  at the existing claim boundary. Preserve the failed binding and append-only
  history; do not manufacture a changed Git input to force a retry.
- Recover original native results only after their exact original admission,
  response and private provenance are complete and positively validated. A
  completed semantic `unknown` judgment is different from missing custody:
  physical file availability must not clear a semantic unknown.
- For verifier infrastructure, reuse the exact original pinned measurement
  operation and its existing resource/storage admission. A generic provider,
  memory or disk observation is not proof that its particular failing operation
  recovered. The unpaid recovery path must structurally refuse all model calls
  and preserve unknown effects, retained sessions and original cleanup duties.
- Positive original-evidence or original-operation recovery can return work to
  normal qualification/correction. It supplies neither intended red, final
  review nor merge approval on its own. Unchanged or unknown recovery remains
  a precise private hold, with no productive reclaim loop.
- Reuse the existing five-minute reconciliation cadence. Bound and fairly
  rotate inspection work so one unresolved request does not block peers or
  final merge reconciliation. Inspection does not create productive progress.

These points are not implementation-ready merely because the source replay is
implemented. The exact recovery descriptor/API, all-cause producer coverage,
original-operation replay recipe and release/reclaim tests remain open. They
must be resolved in code and tests; they are not requests for an owner-selected
limit or a permission gate. Full #1917 host sizing, productive CLI/Discord and
#1703 live/soak acceptance remain separate requirements.

## Open threads from the fresh-eyes checkpoint

All entries are owned by the normal workflow-controller implementation and block
merging #2035 until resolved and tested:

- Recovery authorization needs exact failed-binding identity, permitted issuer,
  invalidation rules and transactional consumption, including duplicate claims
  and crashes. A restored file alone is not an authorization record.
- Original-operation recovery must distinguish inspection, resumption and
  replay. Unknown prior effects must be settled before replay; successful
  cleanup must be positively observed through the existing execution owner.
- Every hold producer needs a typed cause and exit rule. Missing physical
  custody, a completed semantic unknown and infrastructure failure are distinct.
- Historical admission evidence never authorizes new resource-consuming work.
  Original-result inspection is read-only; a measurement replay still needs
  current resource/storage admission while structurally refusing model calls.
- Define the transition for a restored result that is itself negative or
  unknown; custody restoration supplies no positive witness judgment.
- Recovery inspection needs persisted fair rotation and a termination bound,
  preserving time for final merge reconciliation when an inspection stalls.

Pass A checked this document; Pass B confirmed that the no-model requirement,
unknown-effects custody and paid-admission accounting remain explicit. This
checkpoint is not a completed seam audit or an implementation handoff.
