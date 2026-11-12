# Qualification recovery: source data and durable holds

Status: source replay, the unchanged productive-claim fence and original native
custody recovery are implemented locally for PR #2035. Verifier-infrastructure
recovery and fair five-minute wake integration remain load-bearing tasks. This
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
- Native custody uses the protected descriptor and atomic claim described
  below. Infrastructure recovery still needs its operation-specific record.
  Preserve the failed binding and append-only history; do not manufacture a
  changed Git input to force a retry.
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

The native-custody path below does not complete infrastructure recovery. Its
operation-specific descriptor/API, all-cause producer coverage, exact replay
recipe and release/reclaim tests remain open. They
must be resolved in code and tests; they are not requests for an owner-selected
limit or a permission gate. Full #1917 host sizing, productive CLI/Discord and
#1703 live/soak acceptance remain separate requirements.

## Implemented original native-custody transition

The qualification controller classifies a physical private-custody failure or
an unresolved original native admission separately from a semantic unknown.
The existing checkpoint transaction records the hold, its cause and an immutable
descriptor in existing scope-response storage. The descriptor binds workflow,
failed lease generation, complete checkpoint and the exact original native
admission-history digest. A failed descriptor write rolls back the hold;
previously committed original admissions and their charges stay intact.

The existing productive claim can inspect this descriptor without a model or
subprocess API. In the same claim transaction it requires the original cause,
checkpoint, generation and admission history; complete accounting and original
response/provenance pairs; and the normal private-file custody validator.
Owner pause, active lease, pending mutations and uncertain merge dispatch still
block claiming. Missing or invalid evidence yields no claim and no new attempt.

A positive inspection atomically claims the next lease and changes the next
action to `original_native_custody_recovered / requalify_original`, preserving
binding, evidence and accounting. The recovered guard has recovery-only
authority: normal current-policy qualification is still required before a
builder can run. If the worker dies after this transaction, the next normal
claim resumes requalification. If the transaction itself fails, the original
hold and descriptor remain available; recovery was not consumed.

A semantic unknown never creates recovery authorization. A later semantic
unknown in the same lease invalidates the latest physical-cause classification;
a later generation cannot consume the previous generation's descriptor. No
file restoration, owner message or stale descriptor can clear those holds.
Original results and descriptors are retained through the existing store's
lifecycle; this path neither resets budgets nor retires retained sessions.

Tests cover the actual private validator and admission observer, unchanged
failure while the proof is missing, exact restoration with one original model
admission, semantic unknowns, crash resumption, concurrent claims, paused work,
and injected transaction failures. Store-only transaction tests explicitly
script filesystem validation; they do not claim live model acceptance.

## Open threads from the fresh-eyes checkpoint

All entries are owned by the normal workflow-controller implementation and block
merging #2035 until resolved and tested:

- Native recovery authorization now has exact failed-binding identity,
  controller-only cause classification, invalidation and transactional claim
  consumption. Infrastructure needs equivalent operation-specific authority.
- Original-operation recovery must distinguish inspection, resumption and
  replay. Unknown prior effects must be settled before replay; successful
  cleanup must be positively observed through the existing execution owner.
- Native custody preflights now retain a typed physical cause; semantic unknowns
  cannot consume their descriptors. Complete infrastructure producer/exit
  coverage is still required.
- Historical admission evidence never authorizes new resource-consuming work.
  Original-result inspection is read-only; a measurement replay still needs
  current resource/storage admission while structurally refusing model calls.
- Native custody restoration returns only to normal current qualification;
  negative/unknown judgments remain governed by that qualifier. Infrastructure
  recovery must likewise supply no positive witness judgment on its own.
- Recovery inspection needs persisted fair rotation and a termination bound,
  preserving time for final merge reconciliation when an inspection stalls.

Pass A checked this document; Pass B confirmed that the no-model requirement,
unknown-effects custody and paid-admission accounting remain explicit. This
checkpoint is not a completed seam audit or an implementation handoff.
