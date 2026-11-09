# Native admission accounting receipt correlation — bounded C2 prerequisite

Status: scoped design ready for test-first implementation in #1955 after
fresh/source/all 23 mapped seam checks. This is not code approval or rollout. Under #1952 / #1709 / #1706 / #1703; documentation PR #1916.
This scopes one producer interface from the working
[retry lifecycle contract](review-retry-lifecycle-contract.md). All other C2
result-custody, schema, entry-path, hold and wake obligations remain open.

## Source-bound problem and outcome

On main 416f392b, WorkflowGuard::install_model_accounting installs
AuditLogger::set_model_call_observer. The callback generates a model-call UUID,
charges it and persists native_model_admitted in the workflow transaction, but
returns only Result<()>. AuditLogger::record_model_admission then records a
protected model_call_admitted event, optionally with the exact native
resource_identity, without the accounting UUID. No reader can join those two
records by an explicit shared call receipt. Provider equality or event order
must not supply that association.

Outcome: the existing accounting producer returns its own persisted receipt to
the same audit admission call. That protected record carries the exact returned
receipt alongside any existing resource identity. No new call ledger, model
operation, native session, approval or resource-release authority is introduced.

## Closed implementation decisions

D1. Keep one observer slot. Internally its callback returns
Result<Option<String>>. Preserve set_model_call_observer(Fn(&str)->Result<()>)
by adapting a successful old callback to None. Add
set_correlated_model_call_observer(Fn(&str)->Result<String>) that adapts success
to Some(receipt). Both setters replace that same slot, preserving the existing
setter replacement behavior. Never invoke both callbacks or charge twice.
No separate correlated-observer field or second accounting pass. Each admission
clones the current slot under its existing mutex and releases that mutex before
invocation. Replacement affects subsequent slot reads; it does not cancel or
replace an already captured in-flight callback. Preserve this existing snapshot
behavior rather than holding the mutex while callback code runs.

D2. The new callback returns a receipt already accepted by its original
accounting authority. The root workflow callback retains the existing UUID
creation, owner/capacity/purpose/quota checks and charge_admitted_model call,
then returns that exact string after successful commit. The workflow DB schema,
existing native_model_admitted payload, count/attribution readers, charge
idempotence and admission transaction remain unchanged. AuditLogger does not
generate, infer, parse as a native ID or substitute another receipt.

D3. record_model_admission invokes the one observer before appending its
protected audit record. For Some(receipt), require 1..=512 bytes, all ASCII
graphic bytes (no spaces/control characters), before record publication.
Add accounting_schema_version=1 and accounting_receipt=<exact receipt> to
that model_call_admitted record. Preserve actual_provider, resource_identity
and existing admission_schema_version=2 when a resource identity is supplied.
The accounting schema field is independent of the resource-admission version.
Old/unbound observer success adds neither accounting field; absent correlation
remains unknown. Do not upgrade or rewrite old audit events.

D4. Preserve failure ordering. Observer refusal, invalid returned identity or
protected-audit write failure returns Err before ProcessGroup can publish
launch-continue. Observer rejection must not append a successful admission
audit record. A callback may already have durably charged before an error or
before the audit write fails: never refund, decrement or describe that as zero
spend automatically. No atomic transaction across SQLite and audit storage is
claimed. The new fields allow joining successful records; they do not settle
an unmatched record or prove no release, completion or native quiescence.

D5. No full result correlation is claimed. Native session identity and response
custody remain separate. A record lacking resource_identity does not prove a
resource scope, even if accounting_receipt exists. A matching receipt and
resource identity do not prove actual prompt submission, a valid response,
reviewer independence, a successful test or merge authority. Existing resource
admission and final dispatch observers remain mandatory.

D6. Bound changes to tool_audit.rs and self_improve_workflow.rs plus meaningful
existing-boundary tests. Add no user-facing CLI, public/private reporting
payload, env/config setting, storage target, workflow policy or production
capacity change. This scoped producer improvement must not be represented as
C2, #1952 or #1703 completion. Do not edit the active #1954 branch/body,
protected verifier, original request or native runtime.

## Test-first exits

T0. Start with T4's real WorkflowGuard route using only existing APIs: read
its newly persisted native_model_admitted receipt and assert the corresponding
protected model_call_admitted record contains that exact accounting_receipt.
On unchanged main this must fail for the missing field, not a compile error,
new-method absence, fixture setup failure or an unrelated test. Add the new
callback API and its T1–T3 coverage only after that behavioral RED is recorded.

T1. Through AuditLogger's real model admission methods, show the missing explicit
correlation: a new callback's exact receipt must appear with the original
provider and resource_identity in the same protected model_call_admitted event.
Verify the existing accounting callback is invoked once; do not test only a
standalone serializer or source substrings. Record the intended baseline RED.

T2. Old callback and no-observer paths preserve prior fields/counts and emit no
invented accounting identity. Setter replacement uses only the latest callback.
Clone behavior continues to share the single observer slot as before.

T3. Reject empty, 513-byte, whitespace/control/non-ASCII receipts; accept the
exact 512-byte ASCII-graphic boundary. Observer Err or invalid identity must
not append a success record. Verify conservative callback side effects are
not undone by audit refusal. Preserve protected audit-chain integrity.

T4. Use the real WorkflowGuard accounting installation with disposable state,
scripted admission and exact lease/owner fixtures. Verify the UUID in the
workflow native_model_admitted event equals the protected audit receipt,
model_calls increases once, and old quota/purpose/owner refusals remain refused.
No live provider or production state is exercised; a direct scripted admission
is not called an actual native model execution.

T5. Extend the existing real supervised process admission fixture to exercise
the new correlated observer: audit receives the original resource identity and
receipt before launch-continue exists; callback refusal prevents release.
Preserve the existing process-tree cleanup/resource-admission assertions.
Synthetic executable/supervisor fixtures are not live Codex/Claude proof.

Build/test only through scripts/cargo-managed.sh, or immutable hosted CI.
Capture RED/GREEN commands, exact heads and bounded results separately from
independent code approval, normal updater rollout and real native verification.
No original production request is reset to manufacture a test.

## Audit and handoff gate

Verify all producer/consumer sides: old/new setter and clone slot, callback
commit and returned UUID, protected audit encoding/version, resource identity,
error before launch release, ordinary accounting/quotas, and original command
ownership. Fresh-reader, conversation/source comparison and every mapped seam
must complete before coding. Those checks and the precise #1955 lane are now recorded below. #1955 may
implement only D1–D6 with T0–T5; broader C2 handoff remains blocked.


## Design checkpoint

[Audit receipt](1703-admission-accounting-correlation-design-audit.json) records
fresh-reader/source and [all 23 mapped seam checks](1703-admission-accounting-correlation-seam-map.md).
No retained mismatch remains. T0 was clarified to require a runtime missing-field
RED through the existing WorkflowGuard route before adding the new callback API.
Current-source comparison confirmed the original shared-slot snapshot behavior,
charge commit and audit-before-launch ordering. The precise lane is native child #1955 of #1952. Its normal source command
was queued and paused at owner revision 1 to prevent a duplicate issue builder;
read-only verification found zero attempt rows and zero model admissions.
That source hold is not a native teardown receipt. This audit is not code approval, rollout or
completion of C2/native result custody or parent #1703.
