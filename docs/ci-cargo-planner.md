# Protected Cargo planner proposal

Owner: C/#1807, with resource ownership and recovery in R/#1676/#1803.
This refines the candidate in BUILD-VM.md. It is not an implementation handoff,
an enabled privileged execution mode, or a final C proof/API contract. All D1–D8
requirements remain required. The blocking questions at the end must be closed
before production activation; unsupported cases must not fall back to ordinary
candidate-writable Cargo metadata.

## Evidence

The real guest relay fixture at `4e83a9e4` captured 21 direct tool requests,
five compiler units on its first workspace test invocation, and zero compiler
units on its unchanged second invocation. Both invocations and the native signal
probe passed. The fixture includes a Rust build script that invokes a C compiler,
a procedural macro, a library, library tests and documentation tests.

The observed Cargo fingerprint JSON has the exact keys `rustc`, `features`,
`declared_features`, `target`, `profile`, `path`, `deps`, `local`, `rustflags`,
`config`, and `compile_kind`. Test and ordinary-library profiles differ.
Build-script compilation and execution have different fingerprint directories;
the execution record has profile zero and `RerunIfChanged` local entries.
Compiler requests identify procedural macro inputs with `--extern ...=.so` and
carry build-script generated inputs through `OUT_DIR`. The observed jobserver
uses inherited pipe descriptors and `CARGO_MAKEFLAGS`. Rustdoc test requests
execute again on the second invocation even though Cargo compiler units do not.

These are observations from an unprivileged fixture. Its Cargo metadata is
writable by its worker and cannot supply trusted reuse or passing-CI evidence.
Its selected environment trace is not a complete compiler-input closure.

## Decisions and reasons

1. **Separate the planner from application execution.** Cargo runs as non-root
   UID/GID 1001. The existing worker UID 1000 and unit credentials at or above
   2000 cannot write its metadata. The guest controller remains root. Before
   Cargo main, a pinned controller preload installs the existing kernel exec
   denial; interposition delegates supported spawn interfaces, and a missed
   interface fails at the kernel. Cargo itself never runs as root. Protecting
   metadata without preventing application code under the planner credential
   does not establish a boundary.

2. **Use actual execution requests and protected Cargo metadata.** Do not require
   an unstable Cargo unit-graph option or change toolchain channels. A compiler
   request supplies exact argv, environment, working directory and stdin bytes.
   The planner's own fingerprint records supply profile, cfg/features, target
   and compile-kind data. Resolve package/target identities against pinned Cargo
   metadata and actual immutable manifest paths. Compiler output suffixes map to
   their fingerprint directories; a build-script execution maps through its
   compiled executable and `OUT_DIR`. Numeric Cargo dependency fingerprints are
   graph observations, not content proofs. Unknown formats or ambiguous mappings
   produce a typed prerequisite instead of a guessed identity.

3. **Keep logical identity separate from actual inputs.** A unit identity includes
   package/source identity, Rust target and action kind, target triple/spec,
   profile, test/cfg/features, exact effective flags, and runtime/toolchain
   authority. Compiler, build-script compilation, build-script execution and test
   execution are distinct actions. An input manifest separately binds actual
   source/configuration bytes, dependency artifact bytes, environment and frozen
   generated inputs. Changing a dependency's Cargo bookkeeping hash alone must
   not force a new compiler execution when its actual inputs are unchanged.

4. **Replay compatible compiler outputs, never test results.** Before Cargo
   starts, compare protected unit input/output records with the new frozen view.
   Invalidate affected planner fingerprints and transitive dependents so Cargo
   cannot skip required revalidation. If Cargo requests a compiler or build
   script whose complete input identity is unchanged, the relay may publish its
   original sealed compatible outputs without executing it again. Test binaries
   and documentation tests always execute. A reused unit references its original
   producer and cleanup evidence; it does not manufacture a new execution receipt.
   Cross-reconstruction workspace reuse remains deferred. Independently produced
   third-party seeds remain required across reconstructions.

5. **Distinguish precise and opaque producers.** A pinned rustc invocation with
   no untrusted native extension may use its compiler-generated dep-info together
   with exact arguments, environment, configuration, runtime and referenced
   artifacts. A build script, procedural macro, wrapper, custom linker/backend
   or otherwise opaque producer requires its complete permitted visible input
   closure. Candidate-supplied rerun hints cannot reduce that closure. An opaque
   producer cannot attest its own dep-info as a precise closure. Unrelated
   generated inputs must not invalidate an already compatible pure compiler unit.

6. **Remove mutable incremental state from the proposed unit protocol.** The
   proposed planner sets `CARGO_INCREMENTAL=0` consistently before requesting
   units. Reuse of completed compatible Cargo units is still mandatory; within-
   crate incremental compiler state is not required for that reuse. This avoids
   adopting another mutable compiler-input namespace and reduces image growth.
   This is a proposal for the new authority, not a change to current daemon
   settings or permission to delete the existing image's incremental directories.

## Execution and loader protocol

The protected mode must be a separate private controller entry point. The
same-UID experimental Relay and its worker/exec helpers continue to refuse root.
No public flag or application environment can select a root launcher or worker
credential. Root controller interpreters start with a fixed minimal environment;
requested tool environments are applied only after dropping unit credentials.

The proposed root listener owns a root-owned directory accessible for traversal
only by planner group 1001. Its socket is root-owned, group 1001, mode 0660; its
parent is not planner-writable. A sealed startup descriptor binds protocol
version, listener PID/UID, planner UID, private socket and fresh nonce. Both ends
check actual peer credentials and the original command context. Planner loader
and executable bytes belong to the existing measured runtime authority. A
project's environment must not change the planner's own preload or interpreter.

Per-request root helpers are single-threaded before forking credential-dropping
children. They reuse provider-supervisor.py for actual descendant teardown.
Application tools never receive planner control descriptors, planner-writable
metadata, root listener descriptors or storage authority. Jobserver descriptors
are duplicated to stable worker descriptor numbers and their environment fields
are rewritten consistently, so descriptor allocation does not cause false unit
incompatibility. Unknown inherited descriptors are refused.

Stdin must be bounded and frozen before an input manifest is accepted. Spawn
acknowledgement must allow Cargo to finish writing a pipe; it cannot deadlock
while Cargo waits for the exec-start pipe to close. Pending signal/cancellation
handling must cover this staging interval as well as actual execution. Existing
signal tests do not establish this complete protected-mode protocol.

## Private unit records and cleanup

The proposed internal record has these distinct identities: source generation,
original command UUID, logical unit digest, producer UUID, original unit-scope
UUID, input-manifest digest, output-manifest digest, and controller/runtime/cache
identities from the existing C binding. Paths and argv/environment bytes use a
lossless byte encoding; JSON booleans cannot stand in for integer fields.

The controller records the original binding before releasing a unit. Only the
original supervisor's successful stopped/reaped token for that exact scope may
advance it to input capture and output sealing. A generic cleanup string, another
scope's receipt, command completion or whole-VM exit cannot substitute for the
unit token. Outputs become readable to dependent units only after sealing.
Unknown or incomplete records cannot become reusable after a crash.

The command's original guest supervisor must own the root launcher, planner,
relay and all unit supervisors as descendants. The ordinary worker API must not
gain a caller-selectable root UID. A private controller-only scope constructor
must reuse the original supervisor and require an unforgeable in-process token.
Unit scopes form a separate bounded collection; do not put thousands of unit
receipts into the existing command-level maximum of eight scopes. Proposed
limits are four concurrent producers and 4096 unit requests per command; all
storage remains inside the already owned image and its unchanged budget.

An original unit producer record is not a passing CI verdict. C must bind each
required check's exact unit uses, input observations and command cleanup to the
existing coverage producer. The final ObservedInputs constructor and PassingCi
publication remain absent. The exact unit-to-suite schema, required-version
source and trusted validation API must be published and audited before V, M or
baseline consumers are handed implementation work. There is no second binding,
baseline verdict store or merge allowance here.

## Blocking open threads

All entries are owned by C; R owns the existing physical admission and original
cleanup interfaces. None requests a new product decision from the owner.

* **Frozen views and output publication:** specify and test the precise mount,
  projection, output-slot and publication protocol. Existing 0700 UID-1000 source
  exports cannot simply be opened to the planner. Other producers must not alter
  active inputs; worker copies of output artifacts must not become authoritative.
  Close how self-read build-script outputs participate in input identity without
  making every unchanged invocation incompatible. Blocks protected activation.
* **Nested builders and rustdoc:** enumerate actual repository use and define how
  a nested Cargo build obtains complete frozen inputs while its parent producer
  is still running. Do not silently allow a nested unprotected cache or assume
  command-level quiescence freezes an active parent. Define doctest compilation
  versus always-fresh execution under D8. Blocks implementation acceptance.
* **Credential, I/O and cancellation feasibility:** prove the loader/peer/drop
  protocol, stable jobserver mapping, bounded stdin staging, unsupported spawn
  handling, and original supervisor ownership in the real guest. Existing
  same-UID evidence does not prove these transitions. Blocks protected activation.
* **Final unit schema and suite mapping:** turn the fields above into exact
  validated record types and publication/replay rules, including producer-versus-
  reuse records and protected metadata restoration after interrupted Cargo.
  Audit the single C API before consumer handoff. Blocks passing-proof publication.
* **Recovery and retention:** complete unreleased-owner death recovery and bounded
  retention of current dependency indexes, unit payloads, credential slots and
  history. Existing orphan reclamation is credited, not global retention. Do not
  recover by deleting an unknown fence or raising image/resource limits. Blocks
  rollout acceptance and the sustained R criteria.

## Build order and exits

First close the frozen-view, nested-builder and credential protocol questions
against actual fixture/repository behavior, then audit this proposal and the
single C schema before activation or consumer handoff. Add failing tests for
cross-unit metadata writes, old-mtime generated inputs, forged/swapped cleanup,
pipe/signal cancellation, and interrupted preparation/publication. Implement
protected unit production and content invalidation through the existing adapter.
Run the original changed-source/error/assertion and generated/BRS guest replays,
with actual compatible-unit compile counts and trusted external reuse. Only then
connect the final proof producer, V/M/baseline consumers and durable reservation-
to-dispatch fence. Normal matching-build rollout, legacy-executor drain/remote
operation reconciliation, original CLI/Discord merges and the epic's acceptance
soaks remain required. No fixture or deployment alone closes #1703.
