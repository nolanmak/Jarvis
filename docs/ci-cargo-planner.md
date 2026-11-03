# Protected Cargo planner proposal

Owner: C/#1807, with resource ownership and recovery in R/#1676/#1803.
This refines the candidate in BUILD-VM.md. It is not an implementation handoff,
an enabled privileged execution mode, or a final C proof/API contract. All D1–D8
requirements remain required. The blocking questions at the end must be closed
before production activation; unsupported cases must not fall back to ordinary
candidate-writable Cargo metadata.

The authoritative requirements are [#1807 D1–D8](https://github.com/nolanmak/Jarvis/issues/1807)
and [epic #1703](https://github.com/nolanmak/Jarvis/issues/1703), with physical
ownership/recovery in [#1676](https://github.com/nolanmak/Jarvis/issues/1676).
The existing internal proof types are in
[`self_improve_ci_proof.rs`](../crates/augmentagent-cli/src/self_improve_ci_proof.rs),
including `ObservedInputs`, `PassingCiProof` and the single `REQUIRED_VERSION`
constant (currently 1). Coverage is in
[`self_improve_ci_coverage.rs`](../crates/augmentagent-cli/src/self_improve_ci_coverage.rs).
These links identify current groundwork, not a finalized contract or permission
to construct either proof type from the experimental observations.

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

The real module-resolution trace at `ad850efc` additionally contains 1,535
pathname operations: 1,144 failed queries, successful opens/stat/readlink
operations, 18 directory reads and an explicit `/dev/urandom` character-device
open. The decoder now retains successful file descriptor paths, lossless link
bytes and every decoded directory entry, including the end-of-directory result.
It rejects abbreviated entries, unresolved descriptors and inconsistent byte
counts. These facts expose runtime/device and compiler temporary-output inputs;
they do not make those inputs immutable or establish a complete input set.
Device reads must never be converted into ordinary file-content fingerprints.
The captured trace still belongs to the unprivileged fixture, and the original
generated-input invalidation regression remains red.

`scripts/ci-lookup-inputs.py` now selects content/mode facts from the existing
input-census format for individual decoded lookups. It preserves symlink hops
before parent traversal, distinguishes absent components from non-directories,
and binds directory reads to membership only after the original EOF and complete
entry set are present. Unobserved neighbors, unknown devices and writes remain
prerequisites. Changing a selected file at the same old mtime or introducing a
formerly absent candidate changes the selected identity; unrelated generated
files do not. The detached census rejects inconsistent transport and does not
borrow mutable caller lists.

This is a content-selection primitive, not the final C API or a frozen execution
view. It deliberately reports incomplete input closure and no shipping authority.
The trusted collector still must establish the actual immutable unit view,
process attribution, metadata/credential inputs, complete syscall coverage,
producer/output provenance and original quiescence. A prior snapshot and a later
trace cannot substitute for these obligations. No artifact-reuse path consumes
this observation yet.

The source projection now copies literal Git bytes into the original private
generation, packs the reachable controlled Git history without external
alternates, and seals files and directories read-only. Host tests cover SHA-1
and SHA-256 repositories, shallow boundaries, tampering, interrupted allocation,
and replay of the original retirement inventory. The adapter mounts this view
read-only and requires separate original supervisor receipts for fixed access
probes under UID 1001 and UID 2000. Their receipts cannot substitute for command
worker cleanup. The real guest replay at `f4107c1e` passed in 195.55 seconds:
both role probes read the self-contained Git view, failed writes, and supplied
distinct original cleanup scopes. The unchanged compiler unit performed zero
compiler calls. The original generated-module conflict still ran cached `OLD`
while the direct compiler reported E0761. This projection does not freeze
generated inputs or enable protected Cargo execution or passing-CI publication.

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
   no untrusted native extension still needs a proven precise input closure.
   Compiler dep-info alone is insufficient: negative lookups and directory
   membership may change source resolution without changing a previously listed
   file. Do not authorize reuse from dep-info plus unchanged listed files. The
   lookup/namespace rule below blocks this mode until it is proven alongside
   exact arguments, environment, configuration, runtime and referenced artifacts.
   A build script, procedural macro, wrapper, custom linker/backend
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

## Complete replay outcomes and identity bytes

A reusable success contains the entire sealed outcome consumed by Cargo:
normal exit zero, stdout and stderr as exact per-stream byte sequences, compiler
dep-info, generated files, auxiliary output files and their protected manifests.
Build-script stdout includes Cargo directives (cfg, environment, linker settings
and rerun declarations); replaying generated files without those directives is
invalid. Failed, signaled, truncated or incompletely supervised executions remain
historical observations and are not reusable successes. Tests always execute.
Keep the existing combined 8 MiB output bound; exceeding it makes the unit
unverified rather than accepting a truncated outcome. Commit the outcome only
after original unit cleanup and output sealing. A replay must restore all of its
declared outputs before reporting its original status and bytes to Cargo.

Actual argv, source/artifact paths, output suffixes and code-generation flags
remain semantic identity bytes. There is no artifact relocation or rewriting of
`-C metadata`, `extra-filename` or embedded paths. The bookkeeping exception in
Decision 3 concerns only Cargo's numeric dependency-fingerprint observations;
it does not erase a changed argv or artifact identity. Stable `/workspace` and
target aliases must keep an unchanged job's actual invocation stable. Unexpected
allocation paths cannot be silently dropped from identity. Jobserver descriptor
numbers are the one proposed transport normalization: duplicate the same bound
pipe capabilities to fixed worker numbers and rewrite only their defined
jobserver environment fields. Verify this separately before accepting reuse.

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

The proposed protected protocol therefore needs a new version, distinct from the
same-UID version-2 experiment, with `staging_ready` and `tool_started` states.
`staging_ready` means the original supervisor owns the trusted input stager; it
closes Cargo's exec-start pipe so Cargo can write stdin, but attests no compiler
execution. Capture at most 8 MiB of stdin, with a staging deadline of the lesser
of 60 seconds and the remaining declared command deadline. Use bounded streaming
backpressure; EOF is required before accepting the input identity. Seal the
captured bytes, drop tool credentials, and only then emit `tool_started` using
the actual supervised tool identity. The tool execution deadline remains the
existing command deadline, not the 60-second staging limit.

Staging timeout, excess bytes, disconnect or cancellation must tear down the
original stager and planner scope and preserve an unverified attempt. No tool
may start after staging cancellation. Pre-start signals cannot be dropped or
reported as an executed tool's exit; the complete queue/termination behavior is
part of the credential/I/O feasibility gate below. This is the selected state
model; the private implementation below is not authority to activate a
privileged production mode.

`ci-cargo-staging.py` implements the bounded capture and descriptor-normalization
components for that protocol. It reads actual EOF under the lesser of 60 seconds
and the original command deadline, seals at most 8 MiB in a CLOEXEC memfd, and
checks the original cancellation pipe before publishing. It preserves resource
errors and never changes the caller's file flags. Jobserver normalization checks
both directions of the same received kernel pipe, refuses unknown inherited
descriptors or conflicting flags, and rewrites only the defined descriptor
numbers to worker FDs 3 and 4. Other environment bytes remain literal. The pipe's
kernel identity is separate transport evidence, not a semantic compiler input.

Twenty-one real pipe/filesystem tests cover these components. The helper does
not execute tools, provide a complete input closure or publish CI authority; it
is not connected to the existing same-UID version-2 experiment.

The separate V3 native client and root transport now have an unactivated
implementation and a fixed guest probe that has not yet passed a real replay. The
V3 client requires UID/GID 1001 for real/effective/saved credentials, no
supplementary groups, a root-owned sealed startup descriptor and the original
root listener's peer credentials. Protocol version 3 fixes the planner role to
1001; the startup UID field identifies the root listener. V2 continues to
require its same non-root UID and cannot interpret V3 frames.

The root listener uses a root-owned 0710 directory and 0660 socket in group
1001. Each request gets a fresh single-threaded root helper and a distinct unit
UID starting at 2000. Stdin capture and jobserver normalization precede an exec
release gate; the helper drops credentials, rearms parent-death protection and
uses the original supervisor's kernel exec callback. A second release gate
keeps the new executable stopped until the controller publishes tool-started.
Pre-start client signals cancel the request; post-start signals use its original
pidfd. Original per-unit cleanup is required before completion is returned.

The private fixed probe is compile-test-only at the Rust session entry and
cannot be selected by the ordinary CLI or worker environment. Its controller
scope executes only a measured fixed probe with a minimal root environment;
ordinary WorkerScope still refuses UID 0. It tests large raw stdin, actual
jobserver capabilities, planner-file/socket access denial, closed control FDs,
and cancellation through the native V3 client. Six host framing/refusal and
fixture-control tests pass; actual guest credentials and end-to-end transport
are not yet verified.
The first attempted replay at `2e9d1db3` stopped at the host request schema: the
Rust sender included the new private probe field, but the strict adapter did not
allow it. Adapter fixtures now include the Rust sender's ordinary false value,
and malformed or non-repository probe selection fails before runtime loading.
The next replay at `452cb44f` reached the guest but failed. Its bounded phase
was stripped by the Rust diagnostic whitelist; a failing regression now covers
that transport. The fixed probe also had an unconditional parent exit before
stdin streaming. A real fork/exec/pipe fixture test reproduced exit 127 for
ordinary stdin and jobserver modes and passes after correcting the child-only
exit. The host test stubs root-only setup and unit policy assertions; it does
not establish guest isolation. A rebuilt real guest replay is still required.
The probe does not run arbitrary protected Cargo, freeze unit namespaces, seal
reusable outputs, establish a complete input collector or create a CI proof.
Those activation blockers, including race/cancellation and nested-builder
coverage, remain open until their current-build guest evidence exists.

The original descendant supervisor now has a private `on_exec` hook for a
single-threaded trusted bootstrap. That bootstrap arms tracing with TRACEME and
SIGSTOP immediately before its requested exec. The original supervisor enables
TRACEEXEC and EXITKILL, verifies the original thread at the kernel exec event,
and calls the observer while the new tool remains stopped. Only then does it
detach and resume. User SIGTRAP, a closed exec pipe and raw process exit are not
exec observations. The distinction follows the Linux
[ptrace exec-event contract](https://man7.org/linux/man-pages/man2/ptrace.2.html).
The supervisor must itself be single-threaded; it sets a child parent-death
SIGKILL and verifies the original parent before starting the bootstrap. This
covers the interval before tracing can enable EXITKILL. Protected credential
dropping must rearm and verify this guard after credential changes, which clear
it (the Linux [parent-death signal rules](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html));
that transition still needs its actual guest proof.

Thirteen real subprocess tests cover short-lived exec, failed exec, stager death,
forged stops/signals, observer failure, multi-thread refusal, original tracer
death, and staged large stdin/cancellation. Existing supervisor callers retain
their default path. A missing hook invocation remains unverified even when the
bootstrap exits zero. The hook detaches after observation: it is not the complete
syscall/input collector. Protected activation still requires the final launch
and cancellation handshake, credential proof, frozen unit views and collection
armed before any untrusted instruction; this hook alone cannot supply them.
The separate kernel-capability probe now passes machine-word scalar arguments
to the variadic syscall interface. A repeated real-kernel regression reproduced
intermittent EINVAL before the fix; sixteen observations in the regression and
100 direct observations pass afterward. Capability observations still confer no
execution enforcement, complete input closure or CI authority.

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

* **Frozen views and output publication:** finish and test the precise mount,
  output-slot and publication protocol. The sealed self-contained projection
  now supplies read-only Git bytes across credentials. Other producers must not alter
  active inputs; worker copies of output artifacts must not become authoritative.
  Close how self-read build-script outputs participate in input identity without
  making every unchanged invocation incompatible. Blocks protected activation.
* **Precise lookup closure:** select and prove collection of failed lookups,
  successful opens and relevant directory queries for the pinned pure compiler.
  A new generated module candidate is a required regression even when every
  previously read file keeps identical bytes and timestamps. A conservative
  whole-generated-tree hash cannot silently replace this work: D8 still requires
  unrelated generated changes not to rebuild a compatible pure compiler unit.
  Kernel syscall tracing is a candidate to test, not an accepted complete
  collector; truncated traces, unsupported operations or incomplete namespace
  resolution must refuse reuse. Blocks precise-mode implementation acceptance.
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
