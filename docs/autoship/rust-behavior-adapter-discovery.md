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

## Compiler per-file limit and corrected experiment

The [corrected baseline experiment](1703-private-adapter-compiler-limit-observation.json)
finished with fixture failure: Rust could not write its incremental
dep-graph.part.bin because the unchanged 256 MiB RLIMIT_FSIZE returned EFBIG.
No business observation ran. This is not host disk/cache exhaustion, intended
behavioral RED or an enrolled receipt.

The next private helper disables dev/test incremental compilation through Cargo
configuration while preserving debug0, one job, offline/locked mode and the
original kernel/storage limits. It also uses the supported autoship_verifiers
path, discovers Cargo from the existing controller PATH, refuses an ordinary
Git checkout, and rejects writable-source setup before emitting business data.
Both exported production workflow blobs still match their original commits;
the probe is appended only to the disposable runtime copy. Ten exact-parser
controls pass, including valid true/false business data and refused writable
source. These controls do not establish execution of the Rust behavior.
The new original-base run is live at this checkpoint. Original/candidate/mutant
behavior, bounded teardown and independent complete-contract reproduction
remain required before treating the worked proposal as accepted evidence.

## Offline link dependency and observation-only profile

The [no-incremental baseline experiment](1703-private-adapter-offline-link-observation.json)
passed the prior compiler graph limit, then failed linking OrtGetApiBase. The
CLI test binary includes an ONNX dependency whose native runtime is absent in
the offline private Cargo environment. No behavioral probe ran.

Locked offline Cargo metadata and the exact CLI/embeddings package selection
resolve ort/alternative-backend to ort-sys/disable-linking without changing
source or Cargo.lock. The dynamic-loading alternative would need an unlocked
libloading dependency and was not selected. The new private experiment uses
the supported alternative-backend feature only to compile the receipt probe,
which never initializes or calls embeddings. Original production and required
hosted profiles are unchanged; this does not validate embeddings. Source and
file limits, offline/locked mode, no incremental, debug0 and one job remain.

The recipe now carries the Rust launcher, Python helper, observational Rust
module and exact public template as four separately pinned readable files in
controller-supported paths. Module/template bytes match the prior decoded
fixtures exactly. The current original-base run is live; neither this packaging
nor its profile is accepted evidence until actual behavior is observed and
the independent scoper reproduces a complete owner contract.

## Separate C1 provider-boundary gate

Source discovery shows run_impl constructs build_workflow_pinned even when
context.workflow is None. workflow_entry_for checks declared native credentials
and an executable-compatible provider runtime. Replacing CODEX_CLI alone does
not satisfy those preconditions. The verifier sandbox also denies setpgid and
setsid; supervised provider spawning under it must not be assumed to work.
These are source-derived constraints, not an observed provider execution.

Original416f392b embeds the review-group loop;9146acb4 extracts it, and published
3debdb8d uses the extracted adapter. A C1 recipe must identify and validate any
test-only provider instrumentation on the original baseline and candidate,
preserve the actual review/recovery behavior and all source guards, and report
scripted fixture observations without creating production reviewer evidence.
Do not silently substitute9146 as the original baseline, or present the C2
receipt observer as C1 verification. Original #1954 remains paused and preserved.

## Git fixture under the unchanged sandbox

The [four-file/profile experiment](1703-private-adapter-git-fixture-observation.json)
compiled and linked the CLI test, then failed its synthetic Git setup before
receipt accounting. An exact nested-policy replay identifies git add: it cannot
obtain temporary-object randomness through denied /dev/urandom. The getrandom
syscall remains available. No device read grant or sandbox widening was made.

A deterministic canonical SHA1/zlib Git fixture passes real Git read-tree,
rev-parse, clean status, diff, merge-base and strict fsck under the same two
policies. The new private Rust probe seeds those synthetic tree/commit objects
and validates the real repository before using the unchanged WorkflowGuard and
business-observation suffix. It adds no production dependency or lock change.
Its Rust fragment parses through rustfmt. The new original-base run is live;
this fixture validation is not an observed receipt result or native baseline.
The prepared PR1956 body is stale and unsubmitted; the latest readable recipe
still needs actual behavioral evidence and independent complete-contract
construction/reproduction. Original #1954 and the broader goal remain open.
