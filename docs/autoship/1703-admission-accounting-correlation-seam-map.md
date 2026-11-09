S1 · Outer
  A: Status — “Under #1952 / #1709 / #1706 / #1703; documentation PR #1916.”
  B: Referenced issues and PR — “#1952 / #1709 / #1706 / #1703”; “#1916”.
  B: D6 — “must not be represented as C2, #1952 or #1703 completion”.
  B: Audit and handoff gate — “a precise issue lane”.
  Crosses: Parent initiative, documentation ownership, issue lane and completion boundaries.

S2 · Outer
  A: Status — “one producer interface from the working [retry lifecycle contract](review-retry-lifecycle-contract.md)”.
  B: External document — “review-retry-lifecycle-contract.md”.
  B: Status — “All other C2 result-custody, schema, entry-path, hold and wake obligations remain open.”
  B: D5 — “No full result correlation is claimed.”
  B: D6 — “This scoped producer improvement”.
  Crosses: Scope inherited from the retry lifecycle contract and remaining C2 obligations.

S3 · Outer
  A: Source-bound problem and outcome — “On main 416f392b”.
  B: Existing source — “WorkflowGuard::install_model_accounting”; “AuditLogger::set_model_call_observer”; “AuditLogger::record_model_admission”.
  B: D1 — “preserving the existing setter replacement behavior”.
  B: D2 — “retains the existing UUID creation”.
  B: D3 — “existing admission_schema_version=2”.
  B: T2 — “preserve prior fields/counts”.
  B: Build/test instructions — “exact heads”.
  B: Audit and handoff gate — “conversation/source comparison”.
  Crosses: Source baseline and existing behavior against which the contract and evidence are defined.

S4 · Handoff / decision / shared term
  A: D1 — “Internally its callback returns Result<Option<String>>.”
  B: Source-bound problem and outcome — “returns only Result<()>”; “returns its own persisted receipt”.
  B: D1 — “set_model_call_observer(Fn(&str)->Result<()>)”; “successful old callback to None”; “set_correlated_model_call_observer(Fn(&str)->Result<String>)”; “Some(receipt)”.
  B: D2 — “returns a receipt already accepted by its original accounting authority”.
  B: D3 — “For Some(receipt)”; “Old/unbound observer success”.
  B: T1 — “a new callback's exact receipt”.
  B: T2 — “Old callback and no-observer paths”.
  B: T4 — “real WorkflowGuard accounting installation”.
  B: T5 — “new correlated observer”.
  B: Audit and handoff gate — “old/new setter”.
  Crosses: Observer API adaptation and the optional receipt returned to the audit admission caller.

S5 · Decision / shared term
  A: D1 — “Keep one observer slot.”
  B: D1 — “Both setters replace that same slot”; “Never invoke both callbacks or charge twice”; “No separate correlated-observer field or second accounting pass.”
  B: T1 — “existing accounting callback is invoked once”.
  B: T2 — “Setter replacement uses only the latest callback”; “Clone behavior continues to share the single observer slot”.
  B: T4 — “model_calls increases once”.
  B: Audit and handoff gate — “old/new setter and clone slot”.
  Crosses: One shared observer slot, replacement and clone behavior, callback invocation count and charge count.

S6 · Handoff / decision / shared term
  A: D2 — “returns that exact string after successful commit”.
  B: Source-bound problem and outcome — “generates a model-call UUID”; “persists native_model_admitted”; “explicit shared call receipt”; “Provider equality or event order must not supply that association.”
  B: Outcome — “exact returned receipt”.
  B: D2 — “AuditLogger does not generate, infer, parse as a native ID or substitute another receipt.”
  B: D3 — “accounting_receipt=<exact receipt>”.
  B: D4 — “joining successful records”.
  B: T1 — “exact receipt must appear”.
  B: T4 — “UUID in the workflow native_model_admitted event equals the protected audit receipt”.
  B: T5 — “resource identity and receipt”.
  B: Audit and handoff gate — “callback commit and returned UUID”.
  Crosses: Exact receipt identity from accounting UUID generation and durable commit through audit storage and joining.

S7 · Decision / outer
  A: D2 — “owner/capacity/purpose/quota checks and charge_admitted_model call”.
  B: Source-bound problem and outcome — “charges it”; “workflow transaction”.
  B: D2 — “workflow DB schema”; “existing native_model_admitted payload”; “count/attribution readers”; “charge idempotence and admission transaction remain unchanged”.
  B: D6 — “no … workflow policy or production capacity change”.
  B: T4 — “exact lease/owner fixtures”; “model_calls increases once”; “old quota/purpose/owner refusals remain refused”.
  B: Audit and handoff gate — “ordinary accounting/quotas”.
  B: Existing accounting authority — “charge_admitted_model”.
  Crosses: Existing admission authority, quota/ownership/purpose enforcement, charge idempotence, transaction, persisted payload and readers.

S8 · Decision / shared term
  A: D3 — “require 1..=512 bytes, all ASCII graphic bytes (no spaces/control characters)”.
  B: D3 — “before record publication”.
  B: D4 — “invalid returned identity”.
  B: T3 — “Reject empty, 513-byte, whitespace/control/non-ASCII receipts”; “accept the exact 512-byte ASCII-graphic boundary”; “invalid identity”.
  Crosses: Returned receipt validation, byte limits, permitted characters and rejection terminology.

S9 · Handoff / decision / shared term
  A: D3 — “Add accounting_schema_version=1 and accounting_receipt=<exact receipt>”.
  B: Source-bound problem and outcome — “protected model_call_admitted event”.
  B: Outcome — “That protected record carries the exact returned receipt”.
  B: D3 — “that model_call_admitted record”; “Old/unbound observer success adds neither accounting field”; “absent correlation remains unknown”; “Do not upgrade or rewrite old audit events.”
  B: T1 — “in the same protected model_call_admitted event”.
  B: T2 — “emit no invented accounting identity”.
  B: T3 — “must not append a success record”; “protected audit-chain integrity”.
  B: Audit and handoff gate — “protected audit encoding/version”.
  Crosses: Accounting correlation fields, protected event encoding, historical events and absent-correlation meaning.

S10 · Shared term / decision / outer
  A: D3 — “Preserve actual_provider, resource_identity and existing admission_schema_version=2 when a resource identity is supplied.”
  B: Source-bound problem and outcome — “optionally with the exact native resource_identity”; “Provider equality”.
  B: Outcome — “alongside any existing resource identity”.
  B: D3 — “accounting schema field is independent of the resource-admission version”.
  B: D5 — “A record lacking resource_identity does not prove a resource scope”; “A matching receipt and resource identity”.
  B: T1 — “original provider and resource_identity”.
  B: T2 — “preserve prior fields/counts”.
  B: T5 — “original resource identity”.
  B: Audit and handoff gate — “resource identity”.
  Crosses: Provider attribution, optional exact resource identity, resource-admission version and independence from accounting version.

S11 · Handoff / decision / shared term
  A: D3 — “invokes the one observer before appending its protected audit record”.
  B: D2 — “after successful commit”.
  B: D3 — “before record publication”.
  B: D4 — “Observer refusal, invalid returned identity or protected-audit write failure returns Err before ProcessGroup can publish launch-continue”; “Observer rejection must not append a successful admission audit record.”
  B: T3 — “Observer Err or invalid identity must not append a success record”.
  B: T5 — “receipt before launch-continue exists”; “callback refusal prevents release”.
  B: Audit and handoff gate — “error before launch release”.
  B: Existing runtime — “ProcessGroup”; “launch-continue”.
  Crosses: Commit, callback, validation, protected audit append and process release ordering, including failure propagation.

S12 · Decision / outer
  A: D4 — “A callback may already have durably charged before an error or before the audit write fails”.
  B: D2 — “returns that exact string after successful commit”.
  B: D4 — “never refund, decrement or describe that as zero spend automatically”; “No atomic transaction across SQLite and audit storage is claimed”; “do not settle an unmatched record”.
  B: T3 — “conservative callback side effects are not undone by audit refusal”.
  B: External stores — “SQLite and audit storage”.
  Crosses: Durable accounting side effects across the separate audit store, partial failure and unmatched-record interpretation.

S13 · Shared term / decision
  A: Outcome — “No new call ledger, model operation, native session, approval or resource-release authority is introduced.”
  B: D2 — “does not … parse as a native ID”.
  B: D4 — “do not … prove no release, completion or native quiescence”.
  B: D5 — “Native session identity and response custody remain separate”; “do not prove actual prompt submission, a valid response, reviewer independence, a successful test or merge authority”.
  B: Status — “result-custody … remain open”.
  B: T4 — “not called an actual native model execution”.
  B: T5 — “not live Codex/Claude proof”.
  B: Build/test instructions — “independent code approval”; “real native verification”.
  Crosses: Meaning and evidentiary limits of correlated admissions relative to execution, custody, release, quiescence, review and merge authority.

S14 · Decision / outer
  A: D5 — “Existing resource admission and final dispatch observers remain mandatory.”
  B: Outcome — “No new … resource-release authority”.
  B: D4 — “before ProcessGroup can publish launch-continue”.
  B: T5 — “existing real supervised process admission fixture”; “existing process-tree cleanup/resource-admission assertions”.
  B: D6 — “Do not edit … native runtime.”
  B: Existing admission/dispatch mechanisms — “resource admission and final dispatch observers”.
  Crosses: Accounting correlation’s relationship to existing resource admission, dispatch controls, runtime release and cleanup.

S15 · Decision / outer
  A: D6 — “Bound changes to tool_audit.rs and self_improve_workflow.rs plus meaningful existing-boundary tests.”
  B: Source-bound problem and outcome — “WorkflowGuard::install_model_accounting”; “AuditLogger::record_model_admission”.
  B: T1 — “AuditLogger's real model admission methods”.
  B: T4 — “real WorkflowGuard accounting installation”.
  B: T5 — “Extend the existing real supervised process admission fixture”.
  B: External implementation files — “tool_audit.rs”; “self_improve_workflow.rs”.
  Crosses: Authorized production implementation surfaces and existing test boundaries.

S16 · Decision / outer
  A: D6 — “Add no user-facing CLI, public/private reporting payload, env/config setting, storage target, workflow policy or production capacity change.”
  B: Outcome — “No new call ledger, model operation, native session, approval or resource-release authority”.
  B: D2 — “workflow DB schema … remain unchanged”.
  B: D3 — “Add accounting_schema_version=1 and accounting_receipt”.
  B: D6 — “Do not edit the active #1954 branch/body, protected verifier, original request or native runtime.”
  B: Build/test instructions — “No original production request is reset”.
  B: Audit and handoff gate — “original command ownership”.
  B: External protected surfaces — “#1954 branch/body”; “protected verifier”; “original request”; “native runtime”.
  Crosses: Authorized changes, new internal audit fields, excluded interfaces and protected ownership surfaces.

S17 · Handoff / decision
  A: T1 — “show the missing explicit correlation”.
  B: T0 — “Start with T4's real WorkflowGuard route using only existing APIs”; “assert the corresponding protected model_call_admitted record contains that exact accounting_receipt”; “must fail for the missing field, not a compile error, new-method absence, fixture setup failure or an unrelated test”; “Add the new callback API and its T1–T3 coverage only after that behavioral RED is recorded.”
  B: T1 — “do not test only a standalone serializer or source substrings”; “Record the intended baseline RED.”
  B: D1 — “Never invoke both callbacks or charge twice.”
  B: D3 — “that model_call_admitted record”; “Preserve actual_provider, resource_identity”.
  B: Build/test instructions — “Capture RED/GREEN commands, exact heads and bounded results”.
  Crosses: Test-first evidence connecting the original missing correlation to actual admission behavior, event contents and callback count; existing-API runtime baseline RED and its ordering before new API and T1–T3 coverage.

S18 · Handoff / outer
  A: T4 — “Use the real WorkflowGuard accounting installation with disposable state, scripted admission and exact lease/owner fixtures.”
  B: T0 — “Start with T4's real WorkflowGuard route using only existing APIs”; “read its newly persisted native_model_admitted receipt”; “assert the corresponding protected model_call_admitted record contains that exact accounting_receipt”.
  B: D2 — “root workflow callback”; “owner/capacity/purpose/quota checks”; “charge_admitted_model”.
  B: T4 — “UUID … equals the protected audit receipt”; “model_calls increases once”; “old quota/purpose/owner refusals remain refused”; “No live provider or production state is exercised”.
  B: Build/test instructions — “real native verification”; “No original production request is reset”.
  B: Existing accounting installation and fixture/state interfaces — “WorkflowGuard”; “lease/owner fixtures”.
  Crosses: Integrated workflow accounting test setup, authoritative IDs, counts, refusals and boundary between disposable scripted evidence and native verification; reuse of that existing-API route for the runtime missing-field baseline.

S19 · Handoff / outer
  A: T5 — “Extend the existing real supervised process admission fixture”.
  B: D3 — “before appending its protected audit record”.
  B: D4 — “before ProcessGroup can publish launch-continue”.
  B: D5 — “Existing resource admission and final dispatch observers remain mandatory.”
  B: T5 — “audit receives the original resource identity and receipt”; “callback refusal prevents release”; “process-tree cleanup/resource-admission assertions”; “Synthetic executable/supervisor fixtures”.
  B: Existing process fixture/runtime interfaces — “supervised process admission fixture”; “launch-continue”.
  Crosses: Correlated audit admission through supervised process release and retained cleanup/resource assertions.

S20 · Outer / decision
  A: Build/test instructions — “Build/test only through scripts/cargo-managed.sh, or immutable hosted CI.”
  B: T0 — “On unchanged main this must fail for the missing field”; “only after that behavioral RED is recorded”.
  B: T1 — “Record the intended baseline RED.”
  B: T2–T5 — “preserve prior fields/counts”; “Reject empty”; “Use the real WorkflowGuard”; “Extend the existing … fixture”.
  B: D6 — “Add no … storage target”.
  B: External build mechanisms — “scripts/cargo-managed.sh”; “immutable hosted CI”.
  B: Build/test instructions — “Capture RED/GREEN commands, exact heads and bounded results”.
  Crosses: Authorized test execution environment, storage boundary and reproducible verification evidence, including the unchanged-main behavioral RED.

S21 · Handoff / outer / shared term
  A: Build/test instructions — “bounded results separately from independent code approval, normal updater rollout and real native verification”.
  B: Status — “no implementation handoff yet”.
  B: D5 — “do not prove … reviewer independence, a successful test or merge authority”.
  B: D6 — “must not be represented as … completion”.
  B: T4 — “not … an actual native model execution”.
  B: T5 — “not live Codex/Claude proof”.
  B: External later stages — “independent code approval”; “normal updater rollout”; “real native verification”.
  Crosses: Evidence and authority passed from scoped tests toward review, rollout and real native verification.

S22 · Handoff / decision
  A: Audit and handoff gate — “Fresh-reader, conversation/source comparison and every mapped seam must complete before coding.”
  B: Status — “draft contract awaiting fresh/source/seam audit”; “no implementation handoff yet”.
  B: Audit and handoff gate — “Verify all producer/consumer sides”; “This document is a draft and grants no exception until those checks and a precise issue lane are recorded.”
  B: D1–D5 — “one observer slot”; “after successful commit”; “accounting_schema_version=1”; “before … launch-continue”; “remain mandatory”.
  B: D6 — “original request”.
  B: T0 — “using only existing APIs”; “Add the new callback API and its T1–T3 coverage only after that behavioral RED is recorded.”
  B: T1–T5 — “Test-first exits”.
  Crosses: Audit coverage, recorded issue ownership, draft status and prerequisites for implementation handoff, followed by the specified baseline-first implementation sequence.

S23 · Amendment
  A: Source-bound problem and outcome — “returns only Result<()>”; “without the accounting UUID”.
  B: Outcome — “existing accounting producer returns its own persisted receipt”.
  B: D1 — “Internally its callback returns Result<Option<String>>”; “Add set_correlated_model_call_observer”.
  B: D2 — “then returns that exact string after successful commit”.
  B: D3 — “Add accounting_schema_version=1 and accounting_receipt”; “Do not upgrade or rewrite old audit events.”
  B: T1 — “missing explicit correlation”; “intended baseline RED”.
  Crosses: The specified extension from the stated existing callback/audit behavior to receipt-bearing admissions, preserving historical records.

Total: 23 seams.
