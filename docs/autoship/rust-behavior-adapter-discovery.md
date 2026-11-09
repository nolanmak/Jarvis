# Genuine Rust behavioral adapters — source discovery

Status: experimental route to test under #1943 / #1710 / #1952 / #1703,
**not implementation-ready or a registered acceptance proposal**. Original
#1954 request, published source and pinned observer remain preserved.

## Observed failure and existing capability

[The terminal C1 receipt](1703-pr1954-terminal-acceptance-discovery.json) records
13 model admissions, an unmapped assertion-deletion hunk and a negative
independent amendment decision. The authority identifies a source-grep observer
but supplies no complete replacement. The normal scheduled rescope is paused;
no original charge, source, requirement or baseline was reset.

On main416f392b, CONTRACT_PROMPT permits Python, Node or CargoBinary black-box
observers. It describes Rust public-library adapters; it provides no direct
private-unit observational recipe. Legacy CargoTest commands are representable
but controller_asserted returns true only for BlackBox. Test summaries and
exit0 alone are explicitly insufficient. This is a recipe gap to investigate,
not proof that a correct CargoBinary adapter is impossible.

CargoBinary invokes cargo run --locked --quiet for one declared package/bin.
isolated_observation exposes the Rust toolchain only for Cargo invocations,
uses a registered/leased target, keeps original source read-only and permits
writes to its private runtime and target. Expanding Python's PATH is not the
solution. The verifier checkout exporter consumes all regular committed Git
blobs, including a tracked public .env.example. However, exporting a file does
not grant the child read access: the unchanged sandbox source filter also
denies .env.example. The measured denial below corrects the earlier claim that
export alone made this compile-time input usable. This overlaps the input
conflict in #1950/#1951 but does not establish that merging #1951 fixes original
baseline compilation or that credential filters should be widened.

## Existing-mode route to validate before changing the engine

A pinned std-only CargoBinary adapter could invoke a pinned Python helper while
remaining inside the existing Cargo sandbox. The helper would copy the exact
committed snapshot into its private runtime, append a cfg(test)-only
observational module, run one exact Rust test and return one framed behavior
JSON record. An additional nested sandbox would make the copied source and
injected module read-only to compilation/test code, while retaining only the
original target plus separate runtime scratch as writable outputs.

The test module observes actual implementation data; it does not assert an
expected answer or treat a Rust test verdict as the oracle. The existing
controller still compares captured data with its independently registered
OutputOracle. Original sources remain unchanged. A dedicated auto-discovered
src/bin/autoship_*.rs in an already allowed package may avoid a workspace or
lockfile amendment; verify discovery and all scope checks, rather than assume
it. Keep the same pinned fixture and observation semantics for both heads.

[Minimal Cargo-lock observation](1703-nested-cargo-lock-observation.json) proves
that the selected local Cargo run executable can launch cargo check in a
second std-only fixture using the same inherited managed target without timing
out. This used no second target or lease override. It does **not** prove
production-sandbox execution, source-copy integrity or real acceptance.

## Next concrete experiments and carrying costs

1. On private committed exports of416f392b and899e1479, inject a probe using
   existing WorkflowGuard::acquire_at and model_call_admitted APIs. Emit the
   observed workflow charge count and whether its committed receipt appears
   in the actual protected event. Baseline must return valid differing data;
   compilation/setup failure is not this RED. No live provider is needed.
2. Demonstrate the same route under the real verifier sandbox and existing
   resource/storage admission. Read-only injected source, exact package/bin/
   test selection, one bounded behavior record and original target inheritance
   are required. Do not invoke cargo-managed recursively while holding a
   verifier lease or allocate an unregistered target to avoid a Cargo lock.
3. Exercise source-only comment and wrong-receipt mutants, invalid/duplicate/
   missing frames, failed compilation/setup, timeouts and child cleanup. Keep
   fixture errors distinct from genuine observable baseline failure. Preserve
   original source/response provenance; no expected result belongs in the child.
4. Bound copied files/bytes, stdout/stderr and scratch growth; keep inner Cargo
   offline/locked, one build job and dev/test debuginfo disabled. Retain the
   original cache/dependency provenance and positive cleanup rules. Neither
   elapsed time nor a helper exit grants release or a new admission.
5. C2 receipt observation does not certify C1. A C1 adapter must drive actual
   related-source recovery on the original baseline/candidate with the seeded
   distant-item case, full guards and a scripted provider boundary. A new
   helper API absent on the baseline cannot be relabelled as behavioral RED.

Only after these experiments should the independent scoper receive a worked
proposal as untrusted input, preserve every owner requirement, construct an
independently pinned adapter and reproduce original-base behavior. No direct
registration edit, ordinary test-pass waiver, fabricated approval or manual
merge is authorized. A working recipe may avoid a new backend; that decision
remains empirical. Full #1703 acceptance remains open.

## Measured original-base / candidate data and sandbox limit

[Private behavioral observation](1703-private-rust-behavior-observation.json)
records completed managed runs on original416f392b and candidate899e1479. Both
produce one actual workflow charge and one healthy protected admission event.
The original reports receipt_linked=false; the candidate reports true. The
identical cfg(test) module emits values without an expected-answer assertion.
These are ordinary managed experiments, not enrolled native VerifierReceipts.

A direct run of the unchanged first-party sandbox on the committed baseline
export reads env_cfg.rs but returns EACCES for .env.example. That source has
a compile-time include_str of this path. A naive full source copy inside the
observer therefore fails before reaching the behavior under test. Setup failure
must not become behavioral RED. Original baseline, production source and
credential filters remain intact.

The next private experiment carries the exact public template bytes, identical
on both selected heads, as a hashed support fixture. It copies readable source
into disposable runtime and restores only that public fixture, then makes the
copy read-only to the compiler through a nested sandbox. The inner read grant
is limited to that constructed source copy and toolchain/registry; only separate
auxiliary scratch and the original managed target remain writable. This is a
proposal to validate, not an accepted source transformation or production
capability. Source-copy integrity, frame bounds, mutants and actual sandbox
results remain required before a handoff. A successful experiment cannot grant
its own registration, code approval or resource release.

## Fixture-control checkpoint

[Private fixture controls](1703-private-adapter-fixture-controls.json) record
one known invalid experiment setup and exact-parser fault checks. The initial
experiment placed audit control under managed TMPDIR in the writable build
cache; its own verified private process group was deliberately stopped and the
managed handle ended with exit1. It was never behavioral RED. The next run
keeps disposable control outside the target and restores both original
production source blobs, injecting the observational module only into the
private runtime copy. That run is not yet terminal at this checkpoint.

Eight parser controls distinguish valid data from absent/duplicate/malformed
frames, duplicate JSON keys, non-finite values, boolean counters and unknown
fields. A separate counterexample showed writable-source status still emitted
as business data. The staged correction refuses that fixture precondition;
both true and false business values continue to emit actual data. These are
synthetic parser/setup checks, not a native baseline or code approval.

Source comparison also confirms harness_path admits Python only below
autoship_verifiers. The exploratory scripts helper is not installable as a
registered harness. The next staged launcher uses the existing allowed prefix
and portable Cargo lookup from the controller-supplied PATH. Neither helper
placement nor tool discovery is waived because a local prototype can run.
All staged changes must keep the same observation on both heads; an independent
registered proposal still has to reproduce its own original-base behavior.
