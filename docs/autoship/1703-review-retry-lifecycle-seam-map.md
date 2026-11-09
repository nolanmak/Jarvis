# Review retry lifecycle seam map

Status: design-only map of specification boundaries. Judgments are tracked separately in 1703-review-retry-lifecycle-design-audit.json. This map grants no code approval or implementation readiness.

S1 — provenance / scope
A: Contract status: “Under #1952 / #1709,” with scheduling #1706, reporting #1714, acceptance #1715 and parent #1703.
B: Discovery status: “Owner #1709 / #1952, scheduling #1706, parent #1703”; contract status: discovery “remains the provenance for the incident.”
Crosses: issue ownership, parent dependencies, and incident provenance between the two documents.

S2 — amendment / implementation readiness
A: Discovery, “Working C2 decisions after source recheck”: the contract “chooses a separate failed-observation type and pre-dispatch hold identity.”
B: Contract D1–D7; contract closing paragraph: “closes the representation and pre-dispatch placement choices”; both statuses: “not implementation-ready.”
Crosses: source-discovery conclusions become working decisions while unresolved interface obligations remain handoff gates; the linked C1 C2-integration prescription must agree with D1/D3.

S3 — outer boundary / C1 and incident closure
A: Discovery introduction: the coherent source contract “separates successful bounded recovery (C1)” from C2.
B: Both statuses: C1 #1954 has published-input hosted GREEN but its original native attempt is terminal after 13 model admissions and the workflow is normally paused without an active lease; discovery closing: full #1952 closure requires “capture/replay of PR1944’s actual typed request after normal C1 rollout”; discovery boundary 1: “C1’s in-attempt GroupReview retention does not close this seam.”
Crosses: C1 recovery results, C2 lifecycle obligations, rollout order, original command repair, and incident-specific replay.

S4 — shared term / failed observation versus review authority
A: Contract D1: “Failed observations have a separate non-approval type.”
B: Contract D3: facts are not completed fragments or recorded reviews; discovery boundaries 1–3: unavailable results have `structured: None`, callers consume supplied structured evidence, and unavailable `recorded_verdict` is `None`; both behavior-test sections forbid fabricated completion or publication verdicts.
Crosses: historical finding facts, `IndependentReview.available=false`, `recorded_verdict=None`, `approved=false`, structured evidence, passes, adjudication, coverage, completion, and merge authority.

S5 — consumer handoff / adjudicated amendment
A: Contract D1 reason: both resume and adoption call “apply_adjudicated_amendment on structured evidence before unavailable handling.”
B: Discovery final section: “Both callers consult adjudicated amendment before unavailable handling”; contract D1: validator checks “complete roles/coverage and independent final authority.”
Crosses: structured evidence entering amendment evaluation and the complete-role, coverage, and final-authority requirements.

S6 — consumer handoff / episode evidence and publication
A: Discovery boundary 3: source-less adoption sets `previous`, records a verdict only for `Some`, then calls `episode::evaluate`.
B: Contract D1 reason: `episode::evaluate` consumes structured evidence for current authority; discovery boundary 3: evaluator writes `episode-evidence-*` “even when unavailable”; contract exits 1–2 prohibit fabricated structured review or publication verdict.
Crosses: adoption result, previous review state, episode evidence persistence, unavailable status, and publication verdict.

S7 — producer handoff / persist before recovery or bound failure
A: Contract D2: producer is `execute_evidence_group`, “before GroupReview::observe can error and before recover_sources can refuse.”
B: Discovery boundary 1: completed caching occurs only after group success; contract exit 1 and discovery first required test: valid finding followed by denied, missing, or oversized dependency; contract D2 and exit 1: response two survives its bound error.
Crosses: validated native response, original finding, durable observation, dependency resolution, second observation, and group failure.

S8 — decision / valid versus invalid observations
A: Contract D2: persistence follows “native identity, independence, exact binding and response validation.”
B: Contract D2: malformed responses retain “original diagnostic/identity as invalid observations, not valid findings”; open R/E: “strict versioned schemas”; discovery R/E: “native observation proof.”
Crosses: validation result, actual identity, independence, exact binding, diagnostic data, and valid-finding status.

S9 — shared term / original immutable observation custody
A: Contract D2: preserve “original native observation and response digest.”
B: Contract D2: capture “typed missing_sources and original request digest, role, actual native identity and exact ReviewBinding”; contract R/E: “original response/source custody”; discovery first test: original binding, observation identity, and provenance survive restart.
Crosses: original request and response, finding data, typed dependency request, role, identity, source provenance, and exact binding.

S10 — decision / historical resolution
A: Contract D2: later reclassification or resolution is “a separate linked fact.”
B: Contract D3: unchanged heads may supply “labelled prior questions”; changed heads require “current source validation and current observations”; contract R/E and discovery R/E require historical resolution rules or transitions.
Crosses: original observation, linked classification or resolution, head identity, current validation, and fresh reviewer observations.

S11 — ownership handoff / historical storage and reader
A: Contract D3: “existing workflow store/audit ownership” and an “explicit historical-facts reader with source/epoch labels.”
B: Discovery boundary 4: preserved workflow events are not a historical import API; discovery boundary 6: new projections must join existing ownership; contract R/E: retention and growth ownership.
Crosses: durable historical facts, workflow events, store/audit ownership, source and epoch labels, and a reader result that cannot deserialize as current `ReviewEvidence`.

S12 — shared boundary / current review history
A: Contract D3: preserve the reader’s “owner floor, exact binding, pending-owner checks and generation boundaries.”
B: Discovery boundary 4: epoch invalidation advances the floor and clears current indexes; `current_review_history` reads after the floor and stops at generation/owner boundaries; `episode_review_at_head` uses it; `verify_fresh_review_origin` must remain intact.
Crosses: owner epoch, event floor, exact checkpoint binding, pending messages, current review/fragment/ready indexes, current approval history, and fresh-origin validation.

S13 — decision / completed-fragment semantics
A: Discovery boundary 6: `record_review_fragment` checks lease/binding, audit generation, immutable payload, and “8 MiB bound in a transaction.”
B: Contract D3: facts are not `review_fragment` or `review_recorded`; discovery boundary 1: fragment caching follows successful execution; both test sections forbid fabricated completed fragments.
Crosses: successful group completion, transactional fragment storage, payload bounds, failed observations, and uncertain calls.

S14 — entry-path handoff / whole paid pipeline admission
A: Contract D4: hold check precedes “the whole affected paid pipeline.”
B: Discovery boundary 2: QA precedes review; contract D4: resume acceptance/repair and QA, adoption scope registration/conflict repair, fresh issue and repair loops; contract R/S and discovery hold thread: worker scoping, initial/adopted/resumed/repair and owner-follow-up paths; contract exit 3 and discovery third test: zero additional actual admissions.
Crosses: held input state and admission decisions across every affected worker, command, acceptance, repair, QA, scoping, and review entry point.

S15 — scheduling handoff / reconciliation and fairness
A: Contract D4: queue may perform “bounded read-only reconciliation and then yield to other work.”
B: Contract dispatch section: distinguish reconciliation from “new model/verification launches”; discovery hold thread requires worker projections, fairness, and restart behavior; contract D4: held command must not be marked completed or repeatedly consume admissions.
Crosses: command identity, queue state, reconciliation work, yielding, completion state, and model/verification launch permission.

S16 — shared term / pre-dispatch identity versus exact binding
A: Contract D5: “Separate a stable pre-dispatch input identity from exact review evidence.”
B: Contract D5: exact evidence hash includes candidate/caller source, rubric, controller context, executable acceptance, and immutable-write audit findings; discovery boundary 5: existing records lack complete fingerprint; discovery R/P requires exact fingerprint and wake matrix.
Crosses: early no-spend identity, later exact `ReviewBinding`, and the paid work potentially required to construct that binding.

S17 — producer handoff / canonical identity inputs
A: Contract D5: identity is reproducible “without model calls” from canonical repository/workflow and exact source identities.
B: Contract D5 lists owner contract/revision, owner-input boundary, policy/protocol revisions, reviewer/model/rubric identity, and dependency request; contract R/S requires earliest trusted inputs and legacy unknown-state behavior; contract R/E requires canonical hashing; discovery changed-input test varies source, contract, epoch, model, protocol, and policy.
Crosses: canonical identity serialization, trusted producers, revision identities, recorded dependency request, source probes, and missing-key behavior.

S18 — decision / semantic input stability
A: Contract D5: timestamps, lease generation, sessions, notifications, and new request IDs must not “manufacture a new semantic input.”
B: Contract D6 timer/notification/duplicate/same-input recovery row: “zero additional model admissions”; discovery changed-input test adds title and generic status changes; contract exit 4: restarts cannot reset budget.
Crosses: incidental event metadata versus semantic input identity, durable hold state, and allowance continuity.

S19 — decision / deterministic hold versus existing retry state
A: Discovery boundary 5: evidence-budget code has `permanent() == false`; day/headline records are not “an unchanged-input hold.”
B: Contract D4–D6: deterministic hold and typed failures; discovery boundary 5: decomposition, verification retry, and capacity retry have separate domains; discovery third test rejects day counters, sleep, tokens=0, and unbilled labels as substitutes.
Crosses: unavailable reason codes, decomposition state, elapsed-day records, public comments, domain-specific retry mechanisms, and authoritative no-model admission.

S20 — producer handoff / typed failure taxonomy
A: Contract D6: “Use typed producer results rather than error strings.”
B: Discovery R: C1 contextual errors must not authorize persistent holds; discovery transport/admission/storage test; contract D6 distinguishes immutable source failures, selector/item parsing or lookup, evidence/working-set refusal, drift, provider unavailability, storage failure, and native uncertainty.
Crosses: producer failures, deterministic-versus-transient classification, source availability evidence, and hold/retry decisions.

S21 — decision / changed-input wake matrix
A: Contract D6: “A changed key permits read-only revalidation, not automatic paid retry.”
B: Contract matrix source/base/contract and policy/protocol/reviewer rows; discovery R/P asks whether change retries an operation, starts an observation, or first reconciles uncertainty; both changed-input tests require independent variation.
Crosses: semantic input change, owner/source authority, affected request recomputation, actual failure revalidation, historical facts, and eventual admission eligibility.

S22 — outer boundary / owner turn
A: Contract D6 pending-owner row: “Use the existing owner-turn boundary.”
B: Contract D3 current-history pending-owner checks; contract dispatch section: `validate_dispatch` checks owner-turn admission; contract exit 5 and discovery hold thread preserve owner authority; contract D5 includes the owner-input boundary.
Crosses: pending owner input, owner epoch/contract, early identity, current evidence eligibility, and paid-dispatch authority.

S23 — outer boundary / provider, capacity, and fixed evidence limits
A: Contract D6: “A larger host does not increase the fixed review prompt budget.”
B: Contract provider-cooldown row: reconcile provider/native outcome before ordinary admission; discovery boundary 5 separates capacity retries; contract exit 5 preserves capacity refusal and source policy; discovery closing leaves automatic host capacity unfinished.
Crosses: provider readiness, native outcome, evidence hold, host capacity, fixed prompt limits, and ordinary admission.

S24 — concurrency handoff / hold publication and wake claim
A: Contract D7: hold publication, one-wake claim, and admission are fenced by “existing workflow ownership and input revalidation.”
B: Discovery durable hold/claim thread; contract exit 4 and discovery changed-input test require at most one wake under concurrency/restart; contract D7 rejects stale claims and lease expiry as teardown or unspent-call proof.
Crosses: durable held input, claim ownership, current lease/binding, input revalidation, concurrent consumers, restart, and authorized wake consumption.

S25 — outer boundary / GitHub operation ledger
A: Contract D7: operation table models “CreatePr/Push/MergePr/RetirePr/ReadyPr/RetargetPr.”
B: Discovery boundary 6 requires joining existing ownership; discovery final section: table “is not a native observation ledger”; contract D7 requires finding the real native-dispatch/reconciliation producer before transaction design.
Crosses: GitHub mutation operations, native review observations, operation identity, and transaction ownership.

S26 — dispatch handoff / final admission fence
A: Contract dispatch section: `WorkflowGuard::install_model_accounting` installs a dispatch observer calling `WorkflowStore::validate_dispatch`.
B: Contract D4 early unpaid check; contract D7 ownership fencing; dispatch section: `tool_audit::dispatch_allowed` covers model/verification launches and final supervisor release, including asynchronous setup races.
Crosses: early hold decision, current lease/binding, owner-turn admission, final dispatch authorization, launch type, and supervisor release.

S27 — identity handoff / receipt versus native resource
A: Contract dispatch section: model observer creates “a UUID model-call receipt” and charges workflow.
B: Same section: `model_call_admitted_in_scope` records protected `resource_identity`; observer returns only `Result<()>`; contract R/P and discovery R/E require exact observation/dispatch identity and proof.
Crosses: accounting receipt, charge, protected native resource identity, audit record, and the durable call identity needed by `execute_evidence_group`.

S28 — native lifecycle handoff / result custody and uncertainty
A: Contract dispatch section: `fresh_review_scope` returns actual native ID “only on success.”
B: Same section: early `await?` supplies no failure reconciliation handle; `Lease::Drop` uncertainty is in memory; contract D6 lost-response row and discovery failure test require original-operation reconciliation; contract exit 2 requires persistence or blocked reissue.
Crosses: submission, actual native identity, returned result/error, in-memory uncertainty, durable runtime/audit custody, restart, and reconciliation handle.

S29 — decision / audit and storage failure
A: Contract D6: “Storage/audit unavailable” requires refusing publication/new dispatch.
B: Contract D6: missing history is not proof no call occurred; contract exit 5 preserves unavailable-audit guards; discovery failure test includes storage and failed result publication; contract R/E requires crash-safe custody and bounded storage.
Crosses: persistence availability, observed call/result state, publication, dispatch eligibility, and uncertainty.

S30 — shared invariant / authority and resource lifecycle
A: Contract D3: no history event can create reviewer identity, retirement, resource release, allowance credit, or publication verdict.
B: Discovery failure test: absence of result or expired lease proves none of spend, retirement, release, or reissue permission; contract D7 stale-claim rule; discovery boundary 6 forbids parallel session/approval/release ledgers.
Crosses: historical facts, native reviewer identity, spend accounting, retirement, reservations, allowance, release, and current publication authority.

S31 — schema handoff / bounds, conflict, and retention
A: Contract R/E: “strict versioned schemas, canonical hashing, aggregate/per-packet bounds.”
B: Discovery R/E: bounded source/findings and retention guarantees; discovery boundary 6: existing immutable transactional fragment bound; contract R/E adds growth ownership, conflict handling, custody, and “No truncation or arbitrary cache reuse.”
Crosses: persisted packet and aggregate formats, bounded findings/source data, conflicting writes, retention/growth policy, and immutable custody.

S32 — reporting handoff / public and private projections
A: Contract H/N: “public/private compatible payloads and sanitization, wake projections.”
B: Discovery H/N/E requires compatible report schemas and fixtures; discovery reporting test requires CLI/private Discord to show the same durable blocker/wake state with sanitized detail; contract H/N requires actual CLI/private Discord evidence.
Crosses: durable blocker/hold/wake state, report schemas, sanitization, public CLI output, and private Discord output.

S33 — delivery lifecycle decision
A: Contract H/N: “Delivery is an observation, never a hold-clear signal.”
B: Discovery reporting test: “notification delivery cannot clear the hold”; contract D5 excludes progress notifications from semantic identity; D6 notification receipt preserves hold.
Crosses: delivery receipts, notification state, semantic input identity, wake eligibility, and hold clearing.

S34 — acceptance handoff / executable evidence and rollout
A: Contract “Behavior-first exits”: real adapter, crash/reopen, actual entry paths, changed-input matrix, and mandatory guards.
B: Discovery required tests: exact findings, the separate historical-facts reader, actual command worker/admissions, concurrent wakes, injected failures, and real reports; contract E: exact fixtures/call counts, crash tests, source/seam audit, RED/GREEN, hosted CI, independent review, and normal rollout.
Crosses: working decisions into executable acceptance, fixture/event/call-count evidence, independent review, CI, and rollout.

S35 — outer boundary / diagnostic and broader acceptance obligations
A: Discovery reporting test: diagnostic runs reserve “their immutable call budget” and require “fresh actual observations.”
B: Discovery introduction: “No benchmark, approval bypass or allowance reset applies”; contract closing: “No benchmark is active”; discovery closing: all #1703 live acceptance remains separate and unfinished; contract exit 5 preserves exact-head CI, complete independent review, and source policy.
Crosses: diagnostic versus production observations, immutable budgets, approval caches, benchmark status, broader live acceptance, and mandatory rollout guards.

S36 — lifecycle handoff / native retirement, pruning, and durable result custody
A: Contract, “Existing dispatch and uncertainty seams verified,” new retirement paragraph: “NativeSession::Drop also marks registered fresh-review runtime paths retired”; `native_vm::prune_and_count` can delete “unlocked owned state with retirement and completion evidence.”
B: Same section, result-custody interface: “Exact API extension and crash-safe result handoff remain unresolved”; new paragraph: “Durable observation custody and that existing retirement lifecycle must be joined explicitly,” preserving resource/retirement authority without treating a facts event as release proof. Contract behavior-first exit 2: “fresh-scope return/drop followed by normal native pruning before outer publication”; contract D2: persist the original native observation and response digest; contract R/P: original result durability and crash ordering.
Crosses: fresh-scope result return, session drop, native retirement/completion evidence, runtime transcript/export availability, normal pruning, durable original-result custody, outer publication, crash/reopen, and uncertainty that blocks reissue.

This adds the explicit retirement/pruning boundary to the result-custody and resource-authority sides mapped in S28 and S30.

S37 — shared terminology / revalidation claim, eligibility, paid episode, and one wake
A: Contract, “Wake terminology and logical transitions”: a “revalidation claim” owns “one bounded unpaid check”; a “paid episode” executes the affected pipeline under “normal complete call budgets” and may contain several native calls; “one wake” permits at most one paid episode after successful claimed revalidation.
B: Contract D4: hold precedes the whole affected paid pipeline; D6: changed keys authorize read-only revalidation; D7: one-wake claim and native admission are ownership-fenced; behavior-first exits 3–4: zero additional admissions while held and at most one authorized wake under concurrency/restart; discovery changed-input and actual-command tests require exact actual admissions.
Crosses: bounded unpaid checking, successful revalidation, eligibility, one affected pipeline execution, per-purpose call budgets, individual native calls, and the counting unit used by concurrency and no-spend acceptance.

S38 — transition handoff / revalidated eligibility to execution ownership and final dispatch
A: Contract transition table: resolved blocker and reconciled original outcomes yield “eligibility for one existing execution claim bound to the exact revalidated inputs; not dispatch permission”; changed owner/source/policy/input revokes stale eligibility; passing ownership, inputs, and guards permits at most one episode.
B: Same section: eligibility is “consumed atomically with the existing execution ownership” and is neither an owner grant nor a dispatch exception; D5 supplies trusted identity inputs; D6 supplies changed-input revalidation; D7 supplies ownership fencing; existing dispatch section supplies `validate_dispatch`, current lease/binding and owner-turn checks, and final model/verification launch admission; behavior-first exits 4–5 cover concurrent consumers and mandatory guards.
Crosses: exact revalidated input revision, blocker resolution, reconciled original outcomes, eligibility creation/revocation/consumption, existing execution claim, ownership, final dispatch, native resource authority, and purpose-budget admission.

S39 — transition handoff / unpaid probe failure, unknown custody, and crash recovery
A: Contract transition table: untrusted inputs or original custody yield “Recoverable unknown/reconciliation hold; zero new model admissions”; checking/eligible/running crashes require reconciling “original claim and any native submissions” before another episode.
B: Same section: failed probes use “bounded ordinary reconciliation backoff” without a paid episode or erased history; repeated probe receipts do not change semantic identity; exact native correlation and crash-replay transactions remain R/P and R/S gates; D4 permits bounded reconciliation and queue yielding; D5 preserves holds on unreconstructable inputs; D6 requires original-outcome reconciliation and stable semantic identity; D7 rejects lease expiry as teardown/unspent proof; result-custody section and behavior-first exits 2 and 4 cover native uncertainty, crash/reopen, and restart budgets.
Crosses: unpaid probe outcome, trusted-input reconstruction, original result custody, precise blocker, immutable historical facts, reconciliation backoff, claim/controller state, native submissions, crash replay, and permission to seek another paid episode within existing workflow ownership.

Count: 39 seams.
