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
   Every generated/artifact publication must also revalidate stored selectors:
   Cargo may already have cached a fresh decision and never request an affected
   unit. Mark that command's pending invalidations before accepting its result.
   Drain its original planner/unit scopes, invalidate affected fingerprints and
   dependents, then restart the same command with its original binding, stdin,
   coverage invocation and remaining compute/deadline grant. At most four planner
   passes, including the first, are allowed; unresolved or oscillating state
   yields a typed prerequisite. Compatible compilation outcomes may be reused;
   tests execute on each pass. No passing verdict is published while relevant
   invalidations remain pending. A verified red remains actionable only while
   its own observed inputs remain valid. This scheduling protocol is selected
   but unimplemented and requires a real in-command publication regression.

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

7. **Use a separate root and mount/IPC/network namespaces for every unit.**
   Keep stable `/workspace` and target paths, but bind the sealed self-contained
   source view and only the unit's selected artifacts into that root. Runtime
   and toolchain mounts are read-only. Temporary files, Cargo scratch, home,
   shared memory and `/var/tmp` are private to that unit. Unique credentials,
   restricted process visibility and private IPC/network namespaces prevent
   another producer retaining writable output capabilities through shared
   sockets or shared memory. Socket stdio is refused; verified jobserver pipes
   remain explicit transport. Original supervision stays outside the new root.

8. **Mount immutable artifact files without making hard links.** Materialize a
   private directory namespace with read-only per-file mounts from controller
   publications. Hard links change link count and ctime, and reads can change
   atime: those are observable inputs even when bytes stay identical. The host
   regression reproduces those changes; input inventory reads and unit mounts
   now use no-atime semantics. New publications get new immutable inodes and
   index entries. Keep an old publication's original directory entry unchanged until every
   original consumer scope is stopped, not merely until its producer stops.
   Unknown consumer ownership blocks retirement. Directory namespaces are
   private, so a later publication cannot introduce a missing candidate into
   an already running unit's view.

9. **Copy stopped outputs into new controller-owned inodes before publication.**
   After the original unit cleanup is verified, copy only the controller's
   planned output files or declared output roots into a private preparing
   publication. Dynamic build-script filenames are inventoried recursively only
   beneath the originally allocated `OUT_DIR` root, after original cleanup,
   under the same 100,000-entry/4 GiB unit bounds. Reject symlinks, external hard
   links, unmodeled extended attributes, changed files and unsupported entries. Preserve exact
   bytes and executable mode, seal the copy, sync files/directories, rename the
   preparing publication and sync its parent before committing its root-owned
   record. A writer's old inode never becomes the published inode. Interrupted
   copies stay uncommitted; resource exhaustion remains a prerequisite under
   the existing image budget. The logical root/output allocations remain in
   the original managed target, including their preparation records.

10. **Start actual producers with empty own output and scratch roots.** Approved
    source, configuration, dependency artifacts and generated inputs are separate
    immutable mounts. A real build-script execution gets a fresh empty own
    `OUT_DIR`; previous contents are supplied only by a verified outcome replay,
    not restored as mutable initial state. Record initial absence and distinguish
    reads before writes from reads of that execution's own output. The complete
    collector must establish that distinction before reuse. Initial namespace
    identity excludes final outputs, avoiding a circular input/output digest.
    This selects logical initial contents; metadata/runtime equivalence remains
    a separate blocking requirement and cannot be inferred from empty bytes.

### Filesystem feasibility and remaining mapping work

The private fixed guest probe now has an unactivated fifth unit for this
filesystem candidate. It observes the actual original child's distinct mount,
IPC and network namespaces at the kernel exec stop. The tool reads a generated
input while the root controller publishes a different version, then must read
the original bytes and unchanged inode metadata. It also attempts tracked-source
and generated-input writes, a foreign abstract socket, a supervisor-root lookup,
and a private temporary write. After original unit teardown, the controller
copies and seals its one declared output, verifies both inventories and removes
only the original completed probe allocation. The first real VM replay at `d84b19dd` passed in 93.15 seconds, including
actual distinct mount/IPC/network namespaces, generated-file byte and inode
metadata stability, isolation negatives, stopped output publication and original
allocation removal. Its binary SHA-256 is
`e1febea9300eb3664c46e11340db57ca1568d03d280557e7d43780203a034666`.
The guarded replay at `167bc570` passed in 88.15 seconds after adding explicit
extended-attribute and external-inode-alias refusals. Its binary SHA-256 is
`cfb6c7ae6f87b88beb805462327f17cc1749cfdd835c150e669d5152612bb6a3`.
Ten host filesystem tests pass; the integrated harness passes 235 CI Python tests
without skips, 31 focused Rust tests and 96 updater tests. These observations are
mechanics evidence; neither is a trusted input closure or a reusable producer proof.

The prototype uses only a pinned literal artifact and output slot. The actual
Cargo mapper still must resolve each invocation's artifact inputs, output slots,
fingerprints, generated namespaces and exact configuration. It must reserve
fresh writable output slots rather than overwrite a mounted input, and implement
Decision 10's empty own-output/scratch roots and write/read provenance. Unsupported symlinks
and namespace entries currently refuse; repository-required forms need their
own validated policy and behavioral evidence before activation. The private
empty `/cargo` in this fixture does not authorize nested Cargo with unprotected
metadata. Nested builders still require the protected planner and a proven
snapshot/barrier for inputs from an active parent; the existing open thread
remains blocking. Complete collector, sealed stdout/stderr, final unit retention,
original-consumer release recovery and final C proof/API mapping remain absent.

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
implementation and a fixed guest probe that passed a real replay at `f1b4397f`
in 88.59 seconds. This is transport evidence, not protected Cargo activation. The
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
and cancellation through the native V3 client. Eleven host framing/refusal, original-control and
fixture-control tests pass. The real guest receipt verifies UID 2000/2001 tool
execution and UID 2002 cancellation, large raw stdin with identical digests,
jobserver capability preservation, planner-file/socket access denial, closed
control descriptors and original cleanup. The planner-file fixture is not
actual Cargo metadata; its isolation claim remains false.
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
not establish guest isolation. Further guest replays reached the original
kernel exec event, then failed during completion. Bounded diagnostics reproduced
a concrete root control decoder defect: Linux returns MSG_CMSG_CLOEXEC on clean
SOCK_SEQPACKET EOF. The old decoder rejected that EOF as an incomplete frame.
A real socket/pidfd regression fails on the original behavior and passes after
ignoring only that flag for empty EOF. Foreign, short, unknown-stage and truncated
frames still refuse, and their received capabilities are closed. The corrected
real guest replay passed at `f1b4397f`. Its original unit receipts record two
zero exits and an unstarted SIGKILL cancellation, with outer original-scope
quiescence. The pinned test binary SHA-256 is
`cf8610685e7d72fd971d4b72c225a27692897d723c1bddb9f19ded42f4f2d3c1`.
The expanded replay at `e773b37b` passed in 89.93 seconds. A fourth unit under
UID 2003 emits readiness after exec and stdin validation, receives SIGUSR1 through
the original proxy/pidfd, and records raw signal 10 in both tool and proxy status.
The receipt requires that unit to have started and rejects mismatched signal or
exit observations. Its pinned binary SHA-256 is
`c1f350df948561d5a73b2cf35d875a10177d31c27b477f05e4082fa800e19343`.
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
the normal transition is covered by the V3 guest receipt above. Original-parent
death across the credential transition still needs guest fault injection.

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

## In-command generated-publication regression

`live_shared_ci_in_command_generated_publication` adds the scheduling case that
pre-Cargo validation alone cannot cover. Its fixed workspace warms one consumer,
verifies an unchanged repeat performs zero consumer compiler calls, then builds
a publisher whose build dependency is the cached consumer library. Cargo must
resolve that library before the publisher can execute; the publisher also calls
the cached library before introducing the conflict.
The publisher introduces a conflicting generated module with the same old
mtime. The fixture checks that Cargo reported the consumer fresh before the
publication, that all previously read generated files retained bytes and mtimes,
and that the independent compiler rejects the resulting ambiguity with E0761.
The required exit is Cargo/compiler failure, rather than execution of the old
binary. The original between-command regressions remain independent gates.

Two managed host preflights reproduced the false green: one initial consumer
compile, zero on an unchanged repeat, and then zero consumer calls / `OLD` after
the in-command publication. The current fixture uses that explicit dependency order and keeps the consumer
compiler environment identical, selecting the publisher only with the workspace
build command. It requires neither a mutable cross-unit handshake nor a timing
assumption. Worker-owned counters and library-call reports are only fixture
observations; they do not authorize reuse or establish trusted input
closure. The corrected dependency-ordered isolated-adapter run is pending. A successful ordinary
host harness run means the bug was reproduced, not fixed.

## Private unit records and cleanup

The proposed internal record has these distinct identities: source generation,
original command UUID, logical unit digest, producer UUID, original unit-scope
UUID, input-manifest digest, output-manifest digest, and controller/runtime/cache
identities from the existing C binding. Paths and argv/environment bytes use a
lossless byte encoding; JSON booleans cannot stand in for integer fields.

The controller records the original binding before releasing a unit. Before
exec, freeze the initial filesystem view and actual EOF stdin, and arm the full
collector before the first untrusted instruction. Observe accesses and writes
while the original unit runs. Only the original supervisor's stopped/reaped token
for that exact scope permits finalizing the observed-input manifest and sealing
outputs; cleanup is not permission to begin first observation afterward. A
generic cleanup string, another scope's receipt, command completion or whole-VM
exit cannot substitute for that token. Outputs become readable to dependent
units only after sealing. Unknown or incomplete records cannot become reusable
after a crash.

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
* **In-command publication scheduling:** implement Decision 4's bounded planner
  drain/restart under the original command's grant. A producer publishing a new
  conflicting module after Cargo classified its consumer fresh must not leave
  that skipped consumer green. The original negative-lookup regression must
  pass within one Cargo invocation, with compatible compiler counts preserved.
  C owns this gate; it blocks required-check and passing-proof publication.
* **Precise lookup closure:** select and prove collection of failed lookups,
  successful opens and relevant directory queries for the pinned pure compiler.
  A new generated module candidate is a required regression even when every
  previously read file keeps identical bytes and timestamps. A conservative
  whole-generated-tree hash cannot silently replace this work: D8 still requires
  unrelated generated changes not to rebuild a compatible pure compiler unit.
  Kernel syscall tracing is a candidate to test, not an accepted complete
  collector; truncated traces, unsupported operations or incomplete namespace
  resolution must refuse reuse. Blocks precise-mode implementation acceptance.
* **Runtime/device and metadata equivalence:** C must select and prove treatment
  of observed randomness/device access, process/runtime inputs and filesystem
  metadata, including equivalent bytes republished into different inodes. No
  ordinary file digest, ignored stat field or unstated nondeterminism
  normalization can establish compatibility. Affected producers have no reuse
  authority until the rule is proved against D8. The prototype rejects unmodeled
  extended attributes and external inode aliases; this does not settle the full
  metadata policy. This load-bearing gate blocks reuse and activation.
* **Nested builders and rustdoc:** enumerate actual repository use and define how
  a nested Cargo build obtains complete frozen inputs while its parent producer
  is still running. Do not silently allow a nested unprotected cache or assume
  command-level quiescence freezes an active parent. Define parent suspension,
  lineage, slot/jobserver release and reacquisition, recursion limits and
  cancellation before committing the final unit record schema. Four parents
  waiting for nested work must not occupy every slot needed by their children;
  prove the saturated case under the original resource grant. Define doctest
  compilation versus always-fresh execution under D8. C/R own this gate; it
  blocks implementation acceptance.
* **Credential, I/O and cancellation feasibility:** prove the loader/peer/drop
  protocol, stable jobserver mapping, bounded stdin staging, unsupported spawn
  handling, and original supervisor ownership in the real guest. The fixed V3
  replay now proves normal credential transitions, stdin/jobserver transport and
  pre-exec SIGUSR1 cancellation plus post-exec signal/status forwarding. Complete
  signal/race, original-parent death and
  unsupported-spawn fault coverage remain required. Same-UID evidence cannot
  substitute for those guest cases. Blocks protected activation.
* **Protected external-seed import:** use the existing
  [`ci-dependency-store.py`](../scripts/ci-dependency-store.py) `publish`,
  `read_seed`, `lookup_seed` and `install` contracts with the existing
  [`ci-dependency-seed.py`](../scripts/ci-dependency-seed.py) eligibility and
  [`ci-dependency-artifacts.py`](../scripts/ci-dependency-artifacts.py) selection.
  The seed binding, isolated producer, original quiescence acknowledgement and
  per-file/unit integrity remain authoritative. Compatibility revalidates the
  existing authority/cache/contract/dependency identities; workspace source
  history is retained without pretending to certify a new source generation.
  Current `install` uses hard links and sticky root-owned directories; it is not
  the protected planner's restoration path. C/R must prove import into protected
  metadata/artifact views while retaining the original seed reference, exact
  paths/flags, live-consumer metadata stability and the unchanged image budget.
  No direct adoption of candidate-writable fingerprints or new seed authority is
  allowed. The already-passing seed replays are credited; protected restoration
  and required cross-reconstruction reuse remain activation gates.
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

## Filesystem checkpoint audit

Fresh-reader and conversation-to-spec review at the filesystem prototype found
eight claims. The cleanup/capture ordering contradiction is corrected. Bounded
in-command planner restart, declared output-root inventory and empty initial
own-output/scratch contents are now explicit selections whose implementation
still needs behavioral evidence. Runtime/device/metadata compatibility, nested
saturation and protected seed restoration remain named C/R gates above. Their
absence cannot be filled by a passing filesystem fixture. This checkpoint is
not a final schema/API audit, seam audit or consumer implementation handoff.

## Protected Cargo mapping checkpoint

The fixed V3 VM probe now additionally runs a dependency-free Cargo binary twice.
Its root-sealed internal bootstrap gives Cargo UID 1001 and a private planner
target at the original `/workspace/target` path. Each authenticated rustc probe,
compilation and application request uses a separate unit root and UID. The
fixture mapper accepts only its known source, flags, output slots and binary;
unknown mappings hold. This is not the generic Cargo mapper or production mode.

After the original compiler scope stops, output bytes are copied into new
controller-owned sealed inodes. Separate detached copies reach Cargo's target
for its alias/fingerprint bookkeeping. The application receives an immutable
mount of the original sealed executable at Cargo's exact requested alias. Its
attempt to write fingerprint data must affect only its own private target.
Root observations require one compiler execution on the first run, zero on the
unchanged second run, two application executions, unchanged actual fingerprint
contents, sealed artifact integrity and original cleanup. The real admitted VM
replay at `60efd6c7` passed in **98.61 seconds**: one actual compiler invocation
then zero, two application runs, six root-recorded units under UIDs 2005–2010,
preserved actual fingerprints, detached sealed artifact integrity and original
cleanup/allocation removal. The five original transport/filesystem cases also
passed. All generic metadata-isolation/input-closure/shipping claims remain false.

Earlier native runs exposed the installed toolchain's private UID1000/0700
root; read-only mounts of its existing public child directories now expose the
pinned runtime without changing host permissions. `ccd97c8d` then stopped at
`planner_cargo_request` after 92.18 seconds. The exact installed Cargo version
[strips its caller's working-directory prefix from the executable](https://github.com/rust-lang/cargo/blob/29ea6fb6a/src/cargo/ops/cargo_run.rs#L94).
The mapper now accepts that one known relative alias with its original argv and
cwd, mounting the sealed artifact at the same resolved path. Other relative
executables and escaping paths still hold. This virtual runtime root and fixed
mapping are execution prototypes, not the final runtime/metadata-equivalence
policy or a general Cargo mapper.

The in-command publication regression at `43fd246e` did produce a valid native
RED: warm two consumer compiles, unchanged zero, then zero consumer compiles and
OLD output after publication; independent rustc rejected the new input with
E0761. Protected Cargo mapping alone does not fix that failure. Complete input
collection, selective invalidation and bounded planner restart remain required.
All full-CI, trusted-input-closure and shipping-authority claims remain false.

## Declared output-root checkpoint

`copy_output_roots` inventories only the planner's selected producer directories
after original-scope quiescence. It preserves runtime-chosen names, empty nested
directories and roots containing no files. Source roots must not overlap or
escape their allocation; unsupported entries, inode aliases and attributes
remain refusals. Entry/byte bounds cover the union of all selected roots. The
helper rechecks the source inventories after detached copying/sealing; changed
trees cannot return a publication manifest. Sibling artifact inputs and planner
metadata are excluded from the census. Only the original root planner can
establish the required stop barrier; a byte inventory alone supplies no producer
or reuse authority.

The fixed UID2004 VM unit now creates a filename containing its runtime PID and
an empty directory beneath one declared root. The original root callback must
discover and copy both after teardown, verify the exact sealed inventory and
remove the original allocation. The real admitted VM replay at `d4a2d3f6`
passed in **102.05 seconds**, including this exact directory/file inventory,
original cleanup and allocation removal. The same run retained Cargo's one-then-
zero compiler calls, two application runs and actual fingerprint preservation.
There are **246 passing CI Python tests with native helpers and no skips**, and
**31 focused Rust tests pass** (15 opt-in cases excluded). The preceding full
CLI run at `4957bb69`, before the directory-root addition, passed all **1,700**
unit tests (43 opt-in cases excluded) and every integration-test binary; 11
integration cases remained opt-in. Actual Cargo build-script mapping, complete
input collection and publication invalidation remain unfinished. This replay
does not supply trusted input closure, a passing-CI proof or shipping authority.

## Fixed library/build-script graph checkpoint

The next private Cargo probe includes a build-script compiler/execution, a
library consuming its generated Rust source, and a binary linked to the sealed
library. Requests are mapped from measured Cargo 1.94.1 invocations without
rewriting compiler flags, names, cwd or output paths. The build script receives
a fresh writable own `OUT_DIR`; after original cleanup its selected output tree
is inventoried, detached and sealed. Consumers receive a read-only tree mount
and read-only library artifact mounts. Planner copies remain detached. The
build script and program both attempt private fingerprint writes; the program
also checks generated bytes and attempts to write/move its input tree.

Directory censuses now use no-atime directory descriptors. A real host RED
showed `iterdir` advancing the sealed directories' access times; the new scan
preserves them. Root records must require three compiler executions and one
build-script execution on the first Cargo run, zero of either on the unchanged
second run, two program executions, unchanged actual planner fingerprints and
unchanged sealed-tree inode metadata. Both runs share the original admitted
probe deadline. The real admitted VM replay at `0281601e` passed in **104.04
seconds**, with three-then-zero compiler calls, one-then-zero build-script calls,
two program executions and nine original-cleaned tool units under UIDs
2005–2013. Root checks verified unchanged planner fingerprints, detached library
inputs and unchanged sealed generated-tree inode metadata. The program's
positive reads and failed write/rename attempts also passed. **249 CI Python
tests passed with native helpers and no skips**, and **31 focused Rust tests
passed** (15 opt-in cases excluded). This fixed graph is not the general Cargo
mapper, complete input collector, runtime-equivalence policy, in-command
invalidation/restart implementation or final C proof/API. All generic CI,
trusted-input-closure and shipping-authority flags remain false.

## Kernel-copied pathname observation candidate

The existing supervisor observes the original exec stop and then detaches; it
does not collect input queries. Sampling a target's pathname memory before
letting its syscall continue would race with other target threads. Emulating
all metadata calls also introduces target-memory write races described by the
[kernel notification API](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html#NOTES).
The next feasibility probe observes `getname_flags` returns inside the kernel,
then associates the copied name with the original thread's syscall return.
It leaves the target's system call and results unchanged.

The measured root helper starts a fixed bpftrace program in the existing VM.
Before releasing any target unit it requires an actual captured raw-openat
witness from a fixed credential and fresh controller nonce. The selected
records are restricted to the original unit UID range, workspace paths and the
pinned fixture's source spellings. Fixed prefix reads from the kernel-copied
name avoid large temporary strings during filtering; the decoder checks the
full name. Name and return records are emitted separately, joined by original
UID/PID/TID, syscall number and per-thread entry sequence. Delivery across CPU
buffers may reorder the two records; duplicate or incomplete pairs refuse.
JSON framing prevents embedded newlines from creating extra events. A settled
emitted-record counter, bounded parser, original observer exit and original
exec-callback PIDs bind the observations.
Native evidence must include both positive/negative raw opens and the actual
library/build-script/binary source queries plus the library's generated input.
The program/helper bytes join the measured controller bundle; updater detection
must rebuild when the helper changes. No ordinary caller supplies a BPF program
or root execution flag. Original image/resource/deadline limits remain in force.

This is a candidate observation experiment, not a selected complete collector
or final C API. It deliberately does not claim complete syscall/fd/directory/
metadata/runtime coverage or compatibility. The original four attachments now
have the scoped kernel-miss census below; that is not a census of the still
unimplemented general filesystem collector.
Long/unknown path spellings and malformed/lost/count-mismatched observations
have no proof. The existing generic input-closure and shipping flags remain
false. The pinned native feasibility replay below passes; reuse, invalidation
and passing-proof publication remain blocked on the complete collector and
original regressions.

At `7bcba97a`, the native replay passed in **102.08 seconds** with **533 lookup
observations**, the long nonce, the complete 4,095-byte lookup and the original
Cargo graph. Both before/after samples contain the same four program IDs/tags,
four link IDs, observer PID and process birth. The two `getname_flags` return
links each report zero missed calls; all four programs report zero recursion
misses. Compiler calls remain three-to-zero, build-script executions one-to-zero,
and application runs two. Original cleanup, sealed inputs, actual planner and
generated-tree metadata preservation and observer-allocation removal all pass.
**262 CI Python tests passed with native helpers and no skips**; 31 focused Rust
tests passed (15 opt-in cases excluded), including the original exec/relay cases.
Thirteen observer tests reject positive counters, changed owners/objects/tags,
unknown or duplicate objects, missing data and invalid function tables.

The controller requires two original multi-return links plus `sys_enter` and
`sys_exit`. Every link must map to exactly one of the observer process's four
programs. Kernel link info must identify one return attachment at the exact
`getname_flags` address, with the return flag and a readable missed-call counter.
Program FD info must supply the recursion-miss counters; missing fields never
mean zero. These use the kernel's
[link counter interface](https://github.com/torvalds/linux/blob/v6.8/kernel/trace/bpf_trace.c)
and [program miss counters](https://github.com/torvalds/linux/blob/v6.8/kernel/bpf/syscall.c).
The live replay checks their actual ABI on the pinned `7.0.0-31-generic` runtime.
Temporary link-query references are always closed so they cannot preserve an
attachment after the original observer exits. The final census is sampled
after original target-unit drain and before observer detach. All counters must
be zero in both samples; unknown ownership or changed identities refuse.

The supported kernel wildcard matcher is `getname_flags*`; it grants no broader
function coverage because the kernel must report exactly one function and the
original address. Kernel-7 FD info labels these links `kretprobe_multi` and adds
a function table: exactly one zero-cookie `getname_flags+0x0` row is required.
Its multi-return callbacks have a different order from the earlier perf probes.
Emission is registered first so scanning runs first; the actual captured nonce
must establish that order before target execution. The wrong order produced a
valid zero-miss census but no nonce and was correctly held. This is a pinned
runtime observation, not a portable ordering assumption.

Only `missed_probe_census_complete` is now true for this four-attachment capture
window. Full-filesystem coverage, input closure, full CI and shipping authority
remain false. Directory/fd/symlink/metadata/runtime collection, equivalence,
selective invalidation and the original generated-input regressions still gate
activation; this is not the final shared C proof schema/API.

Before that census was added, at `4086c4c9`, the admitted replay passed in **103.29 seconds** on
`7.0.0-31-generic`. Its receipt records 532 kernel-copied lookup observations,
the actual long nonce witness, original-unit positive/negative queries, the
complete 4,095-byte lookup, actual build-script/library/binary source queries
and the library's generated input. Both observer exit and allocation removal
passed. The unchanged graph retained three-to-zero compiler calls and
one-to-zero build-script executions, with two application runs, nine distinct
tool UIDs, original cleanup, detached sealed artifacts and preserved actual
planner/generated-tree metadata. The kernel and helper/program digests bind
this result; it is not a portable kernel ABI claim or a passing-CI capability.
All full-filesystem, missed-probe census, input-closure and shipping flags remain
false. **260 CI Python tests passed with native helpers and no skips**, and 31
focused Rust tests passed (15 opt-in tests excluded). Complete collection,
runtime equivalence, selective invalidation and original regression fixes are
still required before passing-proof publication or autoship activation.

Actual VM startup at `33729c22` failed because the BPF exit probe exceeded the
kernel stack limit. Reducing the capture capacity at `6a49996d` did not resolve
it. Splitting name/result records at `2eeb8162` removed that exit-probe problem
but the name-filter probe still exceeded the limit. Fixed prefix reads at
`a5172687` got past compilation; that replay failed observation validation after
98.92 seconds. The capture capacity is restored to 128 bytes and names reaching
the 127-byte truncation boundary refuse. The seven parser/startup tests and 31
focused Rust tests passed at that revision; these do not establish native
lookup coverage. The captured records still need inspection and correction.

Follow-up capture inspection found both positive and negative original-unit
opens, but bpftrace failed to detach because guest tracefs was absent. The
private observer now mounts tracefs before loading. Directory lookup spellings
ending in `/` are preserved exactly. Rust also queries real output names longer
than 127 bytes, so retaining the old string limit would keep the real graph
unavailable. Names now use sequenced 63-byte chunks, an explicit total length
up to 4,095 bytes, exact offset/length checks and one final sum of emitted
records. Missing or duplicate chunks, truncation, loss and mismatched counters
still refuse. The startup nonce name is longer than 128 bytes; the original
UID-2004 unit additionally issues a 4,095-byte raw lookup whose full bytes and
ENOENT result must be observed before its report can validate.

The initial byte scan hit the kernel's 8,193-jump complexity limit. Word scanning
got farther, but the combined scan/emission program exceeded the
one-million-instruction verifier limit. The current candidate separates those
loops into two return probes, bound by the original TID, syscall-entry sequence
and kernel filename pointer. The pinned native result above establishes ordering
for that graph/runtime; general coverage remains unproved. Startup waits for bpftrace's attachment marker before the
mandatory captured nonce lookup; that marker alone grants no authority.
The strict helper-error decoder accepts only bpftrace's documented null map
lookup/default-zero record. Read, update and delete failures still refuse.
This is explicit map initialization semantics, not an input equivalence rule.
Eleven observer tests pass, including exact bounded loader summaries. Unknown
loader output cannot replace events or the final counter. The private verifier
buffer is 3 MiB inside the existing 4 MiB per-stream log limit; this avoids
losing the kernel's load result to bpftrace's original 1,000,000-byte diagnostic
buffer. Target memory, image, storage, timeout and shipping limits are unchanged.


The directory collector extension requires eight original attachments: the two
raw syscall probes, both original `getname_flags` return probes, and entry/return
pairs for `iterate_dir` and `filldir64`. Before/after program and link identities,
exact function addresses, entry/return flags, and zero loss counters remain
mandatory. The previous four-attachment VM receipt does not validate this new
program.

`getdents64` records bind the original descriptor, buffer capacity, directory
inode/device, kernel actor name bytes, raw and masked type, callback acceptance,
iterator result/final position, and raw syscall result. The decoder reconciles
accepted record lengths with returned bytes and reconstructs copied offsets
from the next accepted actor and final iterator position. It rejects missing,
duplicate, truncated or malformed records. Names are hexadecimal chunks of at
most 63 bytes, including non-UTF-8 names and the full 255-byte component limit.
This captures kernel names and acceptance without sampling a mutable user
buffer. It grants no general filesystem or cache-reuse authority.

Startup must capture both the original missing-path nonce and a complete
`/proc/sys/kernel/random` enumeration through EOF from that same witness PID.
The isolated unit fixture reads a readonly original directory tree with an
empty child, binary filename and 255-byte filename using 288-byte buffers. Its
root controller verifies original inode/type membership, a buffer-full rejected
actor, multiple reads and EOF. Both original Cargo application runs also
enumerate their sealed generated empty directory. Original input metadata,
compiler calls `[3, 0]`, build-script calls `[1, 0]`, distinct unit credentials,
and original descendant cleanup remain required. Full filesystem coverage,
input closure, passing CI and shipping authority remain false. At `c6e542cf`, the real admitted replay passed in **101.82 seconds** on
`7.0.0-31-generic`. It captured **537 path queries, 150 directory reads and
3,885 accepted directory entries**, with all eight original attachments and
programs unchanged and every missed/recursion counter zero. The original
short-read, binary/255-byte names, EOF and both Cargo application directory
witnesses passed. Compiler calls remained `[3, 0]`, build scripts `[1, 0]`,
with two application runs, preserved metadata and original cleanup/allocation
removal. Sixteen observer tests and 265 CI Python tests (native helpers, no
skips) passed at `4c0db70b`; 31 focused Rust tests passed at `c6e542cf` with
15 opt-in cases excluded. Full input closure, passing CI and shipping authority
remain false. This receipt supersedes the four-attachment evidence for directory
coverage only; it does not resolve the original invalidation regressions.


The next private collector revision encodes kernel-copied pathname chunks as
bounded lowercase hexadecimal bytes, using the same `%rh` kernel-buffer format
as directory names. Chunk offsets and declared lengths count bytes; NUL remains
invalid. This removes the old ASCII-only restriction without interpreting name
bytes as output framing, collapsing components or sampling target user memory.
The isolated original unit must open the sealed `raw-\xff` input and query a
missing pathname containing non-UTF-8, newline and space bytes. The report and
original Rust VM test require both original syscall outcomes. The captured
namespace and syscall set remain partial, so complete input coverage and reuse
or shipping authority remain false. At `eff9f4f5`, the admitted VM replay passed in **100.88 seconds** on
`7.0.0-31-generic`, capturing **539 path queries, 150 directory reads and
3,885 entries**. Both binary pathname outcomes, the full 4,095-byte lookup, all
directory witnesses and the unchanged original eight-probe zero-loss census
passed. Compilers remained `[3, 0]`, build scripts `[1, 0]`, with two application
runs, preserved metadata and original cleanup/allocation removal. The full
266-test CI Python suite passed at `8cc37dc2` with native helpers and no skips;
17 observer tests and 31 focused Rust tests passed at `eff9f4f5` (15 opt-in Rust
cases excluded).

The initial variable-size buffer inside every pathname-loop iteration exceeded
the kernel verifier's original 1,000,000-instruction limit. The passing program
uses fixed 63-byte full chunks and only one bounded variable-size tail. It keeps
the original capacity, resource limits, strict helper-error handling and final
counter reconciliation. This proves the bounded binary transport on the pinned
runtime; descriptor resolution, metadata, complete input closure and actual
invalidation/reuse authority still require implementation.


The directory query extension also retains the original file's signed `f_pos`
at `iterate_dir` entry, alongside the iterator's final `ctx.pos`. A complete
controlled enumeration must start at zero and each subsequent read must start
at the preceding final position. Same inode/descriptor numbers and an eventual
EOF alone cannot claim a continuous scan. Raw cookies remain signed 64-bit
values; they are not normalized or assumed monotonic byte offsets.

The original isolated unit now has an additional empty `cursor` directory. It
reads exactly one entry with a 24-byte buffer, calls `lseek(fd, 0, SEEK_SET)` on
the same descriptor, then reads both dot entries and EOF with 288-byte buffers.
The root collector must observe the nonzero first cookie, reset to zero and
subsequent continuous complete read. Both Python validation and the original
Rust VM acceptance require this witness. This tests actual file-position
capture and does not supply generic descriptor-lifecycle or pathname-resolution
authority. At `85ae1b96`, the admitted original replay passed in **103.60 seconds**,
capturing **540 path queries, 153 directory reads and 3,889 entries**. The
read/rewind/full-read/EOF witness and every prior pathname/directory/Cargo
assertion passed with the same eight zero-loss attachments. Compiler calls
stayed `[3, 0]`, build scripts `[1, 0]`, two applications ran, and original
metadata/cleanup/allocation checks passed. The 267-test Python CI suite passed
without skips; 18 observer tests and 31 focused Rust tests passed, with 15
opt-in Rust cases excluded. This remains a bounded observer experiment.


The next private extension observes direct x86-64 `fstat` (syscall 5) through
entry/return probes on the original `vfs_fstat`, plus its original syscall
entry/exit. It records original register/descriptor values, the user namespace,
raw kernel `kstat` fields (device/inode/mode/link count/ownership/rdev/size/block
size/blocks/result mask and signed second/nanosecond timestamps), VFS status
and actual syscall status. Kernel metadata is read before the original kernel
object leaves scope. A successful VFS query followed by a failed copy to the
caller remains undelivered metadata, not a successful stat input. Failed VFS
queries never read an uninitialized result structure. This follows the pinned
[Linux v7.0 fstat path](https://github.com/torvalds/linux/blob/v7.0/fs/stat.c).

Both original `vfs_fstat` attachments must join the same stable zero-loss
census, for ten original programs/links total. Startup must observe fstat on
the original directory witness. The isolated unit must issue direct valid-fd,
invalid-fd and null-output-pointer calls; root validation compares the delivered
metadata with the original sealed inode, and requires EBADF and EFAULT to
retain their distinct outcomes. The original Rust VM test requires all three.
This is not statx/general pathname metadata coverage, FD-lifecycle authority,
a complete input closure or a passing-CI producer.

At `54a6b897`, the admitted original VM replay passed in **105.92 seconds** on
`7.0.0-31-generic`. Its protected receipt records **539 path queries,
153 directory reads, 3,889 directory entries and 653 fstat observations**.
The valid-file metadata comparison, invalid descriptor and failed copy all
passed. All ten original attachments/programs remained unchanged, with every
missed/recursion counter zero. Every prior binary-path, full-length, directory
cursor/EOF, real Cargo reuse, metadata and original cleanup assertion passed;
compiler calls stayed `[3, 0]`, build scripts `[1, 0]`, two applications ran,
and allocations were removed. The 268-test CI Python suite passed with native
helpers and no skips; 19 observer tests and 31 focused Rust tests passed
(15 opt-in Rust cases excluded). Full-filesystem coverage, complete input
closure, passing CI and shipping authority remain false.


The pathname metadata extension observes selected `newfstatat`/`statx` queries
at the original `cp_new_stat`/`cp_statx` entry and return. These kernel objects
already contain the post-lookup mount-ID/attribute fields; observing only
`vfs_getattr` would miss those updates. Original syscall registers bind the
32-bit descriptor, flags and request mask without reading mutable user memory.
The unmodified register values are retained alongside their explicit ABI-width
interpretation. The metadata record joins the original copied-name/result by
UID, PID, TID, syscall and sequence; missing or contradictory records refuse.

The statx record includes raw attributes/mask, birth time, mount ID, change
cookie, subvolume, direct-I/O alignment and atomic-write fields in addition to
the basic metadata. A pinned x86-64 `cp_statx` serializer reproduces all 256
user-copy bytes, including the kernel-only bit masks and zero padding; this is
used only for the controlled witness. Root validation compares its digest with
the original unit's observed buffer. A relative `src/lib.rs` exists in the
sealed dependency directory as well as the tracked working-directory source.
The unit fstats its directory descriptor, then queries that relative path with
both syscalls. The collector must bind the original descriptor, flags/mask,
sealed inode and copy outcome. A missing pathname never invents metadata; an
EFAULT after the copy stage remains undelivered data.

Four copy probes join the same original zero-loss census, for fourteen
attachments. Loader-summary bounds scale with that exact expected attachment
count; the original 4 MiB stream limit, 40,000 event bound, verifier limit and
VM/storage/memory/deadline policy remain unchanged. Metadata is collected only
for selected kernel-copied query names, avoiding an unbounded expansion into
all runtime metadata. Startup requires both original copy paths before target
execution. Generic descriptor lifecycle, concurrent cwd/namespace resolution,
complete filesystem coverage and protected invalidation/reuse authority remain
unproved.

At `153427a2`, the admitted real VM replay passed in **105.49 seconds** on
`7.0.0-31-generic`. The protected receipt records **548 path queries,
153 directory reads, 3,890 entries and 744 metadata observations**. The
descriptor-relative source lookup, exact 256-byte statx copy digest, missing
query and failed copy all passed. All fourteen original programs/links and
their IDs/tags stayed unchanged with zero missed/recursion counts. Compiler
calls remained `[3, 0]`, build scripts `[1, 0]`, both applications ran, metadata
was preserved, and original cleanup/allocation removal passed. The 269-test
CI Python suite passed with native helpers and no skips; 20 observer tests and
31 focused Rust tests passed (15 opt-in Rust cases excluded). This adds actual
query-context evidence, not generic descriptor/cwd resolution, complete input
closure, protected invalidation, passing CI or shipping authority.


The lookup-base extension records the original `path_init` entry/return for
selected kernel-copied query names. The kernel has already selected its root,
working directory or descriptor-backed starting object at this return point.
Records retain attempt order, actual dfd/lookup flags, mount/user namespaces,
initialization status, chosen inode/device/mode/generation, dentry/mount object
identities and the lookup/dentry sequence values. Failed initialization never
invents a chosen object. Nested callbacks are bounded and matched by their
original depth/sequence and kernel pathname pointer; retries remain separate.
A chosen base is not proof that the subsequent path walk completed.

The original isolated unit now queries `src/main.rs` successfully from its
tracked source working directory, changes cwd to the sealed dependency tree,
and repeats that same query where it must return ENOENT. A descriptor-relative
query must use the dependency tree as its base; an invalid descriptor must
report EBADF without a base object. Root validation checks the actual source
or dependency inode and the original unit namespaces, including a consistent
RCU sequence for the controlled immutable objects. The prior cwd is restored
before the remaining fixture runs. Startup requires a base observation for the
original nonce before target execution. Sixteen original attachments/programs
are required, fitting the existing 32-object descriptor census. Diagnostic
attachment counts follow that exact fixed set; stream/event, kernel verifier,
VM, storage, memory and deadline limits remain unchanged.

This establishes selected original starting-object context only. Generic FD
lifecycle, arbitrary symlink/mount traversal and complete input closure remain
unproved; selectors still cannot grant invalidation/reuse, CI or shipping
authority.

At `9c7571bf`, the admitted real VM replay passed in **104.14 seconds** on
`7.0.0-31-generic`. Its protected receipt records **553 copied-name queries
and 553 original lookup-initialization attempts**, plus 153 directory reads,
3,890 entries and 748 metadata observations. The same relative pathname
succeeded from the source root and failed after cwd changed to the dependency
tree; descriptor-relative and invalid-descriptor cases also matched their
original kernel bases/outcomes. All sixteen original programs/links and their
IDs/tags remained unchanged with zero missed/recursion counts. Prior binary
paths, full length, directory cursor/EOF, metadata copies, compiler `[3, 0]`,
build scripts `[1, 0]`, two applications, metadata preservation and original
cleanup/allocation removal passed. The 270-test CI Python suite passed with
native helpers and no skips; 21 observer tests and 31 focused Rust tests passed
(15 opt-in Rust cases excluded). Full filesystem/input closure, invalidation,
passing CI and shipping authority remain false.


The frozen-view/content bridge connects selected original kernel query records
to `ci-lookup-inputs.py`. Root creates the index before target execution from
sealed byte-path inventories, retaining original inode identities and explicit
virtual prefixes. Routing-only ancestors do not attest unknown neighbors.
Ambiguous aliases, changed inventories/inodes, foreign namespaces, unsupported
query flags, unresolved bases and contradictory syscall outcomes remain typed
input-census prerequisites. Records are detached from caller mutation; binary
filenames remain byte data. This is not the final shared C input-proof API.

The controlled bridge consumes both original `src/main.rs` queries: success
from the source root and ENOENT after cwd changes to the dependency tree. It
requires their actual kernel base, metadata and namespaces rather than a caller
cwd fallback. The selected content digest is separate from its execution-context
binding. Root then refreshes the original virtual selectors against two new
sealed views: changing an unread file must keep the selected content identity,
while introducing the missing module at an old timestamp must change it. The
original frozen inputs remain intact. Refresh is a relevance observation, not a
new execution receipt or permission to replay an artifact.

`ci-lookup-inputs.py` joins the measured private planner bundle and rebuild
triggers. The original sixteen kernel attachments and all resource/storage
limits remain unchanged. The original Rust VM acceptance requires the bound
selector and both content-identity comparisons. Complete input coverage,
metadata/runtime compatibility, generic Cargo mapping, invalidation/restart,
passing-CI publication and merge authority remain unfinished. Native replay
of this bridge passed at `b75f9da1` in **106.18 seconds** on
`7.0.0-31-generic`. The protected receipt records two actual bound queries:
the original content digest equals the unread-change digest and differs from
the introduced-module digest. The original sixteen attachments/programs stayed
unchanged with zero missed/recursion counts, and all prior path/directory,
metadata, Cargo reuse and original cleanup checks passed. The 272-test CI
Python suite passed with native helpers and no skips; 10 lookup-input tests,
21 observer tests and 31 focused Rust tests passed (15 opt-in Rust cases
excluded). All 99 updater rebuild checks passed. The input-closure and shipping
flags remain false; the content comparison does not establish metadata/runtime
compatibility or schedule a Cargo invalidation/restart yet.

The next invalidation component is a command-bound durable intent journal in
`ci-lookup-inputs.py`. A changed selected-input digest marks the unit and its
transitive dependents pending before the caller can act. Restarting the
controller or observing the original digest again cannot clear pending work.
Late dependents inherit pending state. Private, atomically replaced records
retain the original binding, digests and owned fingerprint paths; malformed
graphs, path escapes, duplicate ownership, stale writers and rebound commands
are refused. This component does not delete Cargo metadata, prove original
scope drainage, execute retries or issue passing-CI evidence. The real compiler
selector and planner retry path still need to consume it. Its regression suite
passes alongside all 275 Python CI tests with native helpers and no skips;
focused managed Rust checks also pass. No new native VM result or production
activation is claimed for this journal addition.

The fixed private Cargo replay now connects the journal to the original library
compiler's negative lookup for a generated `chosen.rs`. The build script emits
`mod.rs` and `chosen/mod.rs`; the application still checks the exact generated
bytes, value 7, empty directory, denied writes and isolated Cargo metadata.
Root captures the unit's kernel root object at the supervised exec stop, before
releasing user code. Absolute lookup names can route through that original
object only into explicitly declared frozen trees. After the original scopes
drain, a separate sealed view introduces `chosen.rs` at an old timestamp.
Refreshing the actual compiler selector must change its content digest and
durably mark both library and binary pending, while preserving the old sealed
publication and metadata. This extends the diagnostic acceptance, not the final
input-proof API. Actual fingerprint deletion, compiler retry and the original
in-command regression remain unfinished.

The first real compiler replay exposed a main-thread-only selector assumption:
rustc's actual negative `statx` query came from a worker TID within the original
supervised process. The retained kernel records preserve its TGID, TID, base
object and namespaces. Explicit threaded selection now accepts that shape while
still requiring the original TGID/UID, exact per-query metadata join, original
root object and both namespaces. Foreign processes/namespaces remain refused;
the default controlled-probe selector remains main-thread-only. The failed
native run is retained as diagnostic evidence and is not a passing receipt.

The corrected native replay at `79c8572d` passed in **109.37 seconds**. Its
protected receipt contains one actual UID2010 library query, different original
and refreshed selector digests, and durable `binary`/`library` pending records.
The original generated publication and metadata remain unchanged. Cold/warm
compiler counts remain `[3,0]`, build-script counts `[1,0]`, applications 2;
all nine Cargo unit scopes have original cleanup receipts. The original sixteen
kernel programs/attachments and zero-miss census remain required. The replay
observed 558 copied-name/base queries, 750 metadata queries and 153 directory
reads. All 277 Python CI tests and 31 focused managed Rust tests passed (15
opt-in Rust cases excluded); the updater checks remain 99/99. Full input closure,
passing-CI and shipping authority remain false, and this result does not prove
fingerprint invalidation, command restart, or the in-command regression.

The next private replay step runs each Cargo invocation under its own original
root supervisor. The controller validates the bound private cleanup receipt and
joins all relay handlers before acting on pending work. It persists an exact
fingerprint-retirement intent, compares the current owned metadata inventory,
and removes only the library/binary directories named by that journal. Unknown
paths, changed inventories, aliases and missing cleanup evidence refuse the
operation. Unaffected fingerprint contents and historical sealed publications
must remain intact.

The same fixed Cargo command then runs a third time against the introduced
generated publication. Acceptance requires one actual library compile, rustc
exit 1 with E0761, Cargo exit 101, no build-script/application execution, and
original cleanup for the failed compiler and planner. Failed compiler outputs
are not published and pending state is never cleared. The existing successful
cold/warm counts and exact-byte assertions remain unchanged. This is a private
between-command invalidation diagnostic, not the original in-command restart
exit or a passing-CI producer.

Native replay at `7cc8ddff` passed in **108.90 seconds**. After original planner
and tool cleanup, the exact journal-owned fingerprints were retired and the
same Cargo command actually compiled the affected library. Rustc returned 1
with E0761 and Cargo returned 101; the dependent binary stayed pending and did
not execute. Unrelated fingerprints, original generated bytes/metadata and
historical sealed outputs were preserved. The old successful cold/warm compiler
counts `[3,0]`, build-script counts `[1,0]`, and two application runs remain
unchanged. All 281 Python CI tests and 31 focused managed Rust tests passed
(15 opt-in Rust cases excluded). The sixteen original kernel programs and links
were unchanged in the before/after zero-miss census. The failed retry is a real
diagnostic compiler result, not a passing-CI capability: complete input closure,
generic unit mapping, publication-time bounded restart, the original in-command
regression and all merge/rollout acceptance remain unfinished.

The guest/host diagnostic filters now retain the fixed supervisor, fingerprint
retirement and retry stages instead of reducing them to `unknown`. Python and
Rust boundary tests require those stages and errno while discarding private
payload fields. The 281-test Python CI suite and 31 focused managed Rust tests
pass after this diagnostic-only follow-up; the separate VM transport suite
passes 14 cases with its six preexisting provisioned-runtime cases excluded.

Direct rustc request parsing is now shared by the pinned replay and a bounded
unit mapper. It keeps workspace-member source paths, repeated cfg/check-cfg and
extern arguments, check/test action differences, profile and explicit Linux
target paths, generated roots and exact request-context identity. Output paths
are derived only for supported library, metadata-check, binary, test and
build-script compilation forms. Response files, escaped paths, incremental
state, unmodeled native/opaque tools and ambiguous singleton options hold.
The pinned replay still applies its original exact package/flag/source/output
restrictions after shared parsing; this does not widen its execution admission.
The mapper's request digest is not a compatible-unit key or input proof. The
caller must still admit the package graph, toolchain and every publication.
Proc macros, rustdoc, registry-source mounts, wrappers and general execution
dispatch remain unfinished.

The shared-parser refactor at `025b8b48` passed the original protected VM replay
in **110.42 seconds**. It preserved cold/warm compiler counts `[3,0]`, build
script counts `[1,0]`, two application runs, durable selected invalidation and
the actual one-compiler E0761 retry. Original scopes, sealed bytes/metadata,
unrelated fingerprints and all sixteen kernel programs/links remained intact.
All 284 Python CI tests and 31 focused managed Rust tests passed (15 opt-in
cases excluded). This native evidence exercises the original pinned graph;
the new check/test/target mapping cases have host parser tests, not general
execution admission or complete input/reuse proof. The original in-command
fixture and production activation remain unfinished.

Workspace target admission now consumes actual pinned Cargo metadata rather
than inferring package identity from filenames. The private controller runs
`cargo metadata --offline --locked --no-deps --format-version 1` under its own
original supervisor and permits only information probes during that operation.
It binds metadata package IDs, versions, manifest paths, target names/kinds,
editions and source paths to sealed owner-checked files and parent directories.
Each subsequent compiler request must match the package environment and exactly
one declared target. Changed bytes at an old timestamp, writable/replaced
parents, duplicate/ambiguous targets, foreign workspace paths and mismatched
versions hold. Metadata and request hashes remain observations, not an input
closure or reuse proof; original Cargo execution/cleanup provenance is required
from the caller.

The existing pinned replay now requires graph admission for all three initial
compilers and the failed library retry. Its original cold/warm and error
criteria remain unchanged. `--no-deps` does not admit registry dependencies;
opaque tools, complete input closure, general graph execution dispatch,
publication-time restart and the original in-command fixture remain unfinished.
Native replay at `e7d6af74` passed in **109.98 seconds**. The protected receipt
binds one metadata package and three declared targets to all four actual
compiler admissions: build-script, library, binary and failed library retry.
Original metadata cleanup is verified separately. The existing successful
cold/warm counts `[3,0]`, build-script counts `[1,0]`, two applications and actual
E0761/Cargo101 retry all remain intact, together with unchanged original kernel
programs/links and sealed-publication/metadata preservation. All 288 Python CI
tests and 31 focused managed Rust tests passed (15 opt-in Rust cases excluded).
This evidence exercises the pinned workspace and does not admit registry
dependencies, general graph dispatch, complete input closure or shipping.

The pinned replay now routes requests through a shared graph-backed dispatcher.
Compiler requests use admitted targets; dependency artifacts are selected from
their actual owned publication rather than a global library label. Executables
must match exactly one owned detached artifact, preserve the requested alias
and argv, and still match its declared frozen target and producer action.
Generated input roots must match one publication owned by the requesting
package; a binary cannot acquire build-script output permissions through a
contradictory producer record. Publication records now retain action, target
binding and generated-root context, and historical-state comparisons include
those fields. Root verifies sealed bytes/inventories again before mounting.

The private fixture guard still applies its original package/source/flag
restrictions. Publication storage is still the pinned replay's bounded storage;
multiple-package producer lifetimes, opaque tools, general CI entry dispatch,
complete input closure and publication-time restart remain unfinished. This
dispatcher is an execution-routing component, not an artifact compatibility or
passing-CI capability.

Native replay at `58e1b288` passed in **110.93 seconds** with graph-backed
dispatch required by the protected receipt. All four compiler requests and the
owned build-script/application execution path used the dispatcher. Original
cold/warm counts `[3,0]`, build-script counts `[1,0]`, two applications and the
actual E0761/Cargo101 retry remained intact. Source/generated bytes, metadata,
historical publication context, original cleanup and all sixteen zero-miss
kernel programs/links stayed verified. All 290 Python CI tests and 31 focused
managed Rust tests passed (15 opt-in cases excluded). General CI activation,
multi-package publication lifetimes and the original in-command restart exit
remain unfinished; no shipping authority is claimed.

Publication storage now uses per-execution producer IDs, protected records and
separate active file/namespace indexes. Role names remain diagnostic aliases
only. Each actual producer requires a successful original unit exit and cleanup
receipt bound to this command; a root-created relevance snapshot is explicitly
`controller-diagnostic` with no execution claim. Old sealed objects and producer
records survive namespace replacement. Cross-producer collisions and overlapping
namespaces hold. A prepared transition is durable before selector callbacks;
callbacks persist affected units before the active index changes. Failed
callbacks/materialization leave the store unusable and recovery does not
silently recreate an empty store. Detached planner copies are replaced atomically
after ownership/content validation, never by mutating sealed history.

The private replay now publishes its introduced-module snapshot through this
transition path: the actual compiler selector refresh marks library/binary
pending before activation, then the same Cargo command exercises the selective
failed rebuild. A durable restart gate binds the original bootstrap context and
deadline, requires original Cargo cleanup between passes, and refuses more than
four passes or a changed grant after reopening. The unchanged warm command and
its failed retry use two passes in this diagnostic; this does not prove the
original in-command publication harness. Live selector recovery, opaque tools,
general CI entry dispatch, full input closure and final passing-CI publication
remain unfinished.

Native replay at `147c891c` passed in **113.90 seconds**. The protected receipt
contains five preserved producer records: four actually executed units and one
explicit root-created diagnostic snapshot. Four producers remain active after
the namespace transition. The selector callback durably marked library/binary
pending before activation; the failed library rebuild then ran as pass two
under the unchanged bootstrap grant and four-pass ceiling. Original cold/warm
counts `[3,0]`, build-script counts `[1,0]`, two applications, E0761/Cargo101,
original cleanup, sealed history/metadata and sixteen zero-miss kernel
programs/links remained verified. All 296 Python CI tests and 31 focused managed
Rust tests passed (15 opt-in cases excluded). This is the integrated controlled
replay, not the original in-command publication exit or production activation.

The workspace executor is now separated from the private fixture guard. An
internal test-session switch connects it to the actual `targeted`/`baseline`
adapter, replacing that operation's ordinary Cargo driver. A single controller
function constructs both drivers' original argv, including package selection,
filters and test flags. The protected path captures the worker environment and
cwd, adds only its private transport, supervises original Cargo and unit trees,
and preserves ordinary command failures and streams. Stream capture is bounded
while the command runs. Its execution receipt is tied to the same command and
worker scope as output observation; it is not a complete-input or passing-CI
proof. The exact private source/body guard and probe-only output assertions stay
in the probe subclass.

This connection remains disabled in production builds. A predecessor command
currently holds because persistent selector/publication recovery is unfinished;
it cannot acquire another cold target as a substitute for D8. The structured
path still holds unsupported compiler, registry, proc-macro, rustdoc and opaque
wrapper forms. Repository-shell/nested Cargo entry and the original unchanged
in-command generated-publication regression remain required. The added native
adapter test uses an independent two-package graph, preserves normal build/test
argv, checks unchanged dependency/library reuse between those commands, and
requires an original failing Rust test to propagate Cargo101 and its diagnostic.
Native validation at `bdcb1a88` passed in **182.94 seconds** through the actual
adapter. Both source cases compiled two library units for the original build,
then only the new test unit for the original test command. The successful test
returned zero; the intentionally failing test returned actual Cargo101 and its
original diagnostic. Original argv, output and independent worker cleanup were
preserved. Normal Cargo's bare `rustc` requests are admitted only with the pinned
toolchain first on the captured PATH, while actual execution requests remain
unchanged. Writable/reordered search paths hold. The fixture lockfile uses
canonical bytes generated through the managed Cargo wrapper, so the immutable
source view does not need permission to rewrite it.

The original publication/restart replay also passed after extraction in
**109.21 seconds**, preserving cold/warm compiler `[3,0]`, build-script `[1,0]`,
the actual E0761/Cargo101 retry, pre-activation invalidation, sealed history,
original cleanup and the sixteen-program/link zero-miss census. All **302 Python
CI tests passed without skips**, and **31 focused Rust tests passed** with 16
opt-in cases excluded. The structured adapter and compatibility VM cases were
then explicitly executed. This does not prove the original in-command harness,
persistent reuse or complete input/CI authority. No restoration is deployed.

Repository-command routing now has a distinct sealed worker role (`JARVWRK3`,
UID/GID 1000), separate from the planner role (`JARVCFG3`, UID/GID 1001). Root
still owns both descriptors and authenticates the listener peer. Worker children
retain their worker credential and execution guard. Only pinned Cargo requests
enter the existing protected planner namespace; their exact original request,
stdin and descriptor mapping remain staged under the original supervisor.
Terminal Cargo evidence is completed and its slot released before replying, so
an immediately following unchanged command cannot race the prior slot cleanup.

This path is exposed through the actual repository adapter behind an internal
test-session switch. It remains disabled in production. The currently admitted
worker runtimes are pinned shell/Python executables from read-only runtime
mounts, with a supported ELF interpreter. Static/opaque executables and loader
redirects hold rather than running without the preload's kernel guard. Requests with an observed namespace mismatch or mismatched
per-message sender credentials hold. The shared listener parent is traversable
without being listable or writable; child directories and sockets retain
separate credential permissions. The private protocol replay additionally
requires both crossed worker/planner descriptor roles to exit before reaching
main. These assertions now pass in the native replay described below.

The new repository VM test exercises Python spawning Cargo three times. It
requires a real first compilation despite forged worker-owned fingerprints,
zero compiler calls for the unchanged build, and a real test compilation/run.
It also attempts direct exec syscalls and traversal into planner metadata.
These assertions do not replace the unchanged original in-command publication
regression. Persistent predecessor recovery, frozen generated forests, the opaque
counter-wrapper graph, broader tools and complete input/CI authority remain
unfinished; repository reentry with a predecessor is still explicitly held.


Native validation at `a5eed6ec` passed in **101.86 seconds** through the actual
repository adapter. The original Python driver ran under UID/GID 1000 and three
original Cargo processes ran under UID/GID 1001. Compiler calls were `[1,0,1]`:
real first build despite worker-forged fingerprints, unchanged build reuse, and
real test compilation/execution. Direct worker exec syscalls were denied, planner
metadata traversal was denied, original worker/Cargo cleanup was verified, and
original stdout/stderr remained intact. No generated forest, opaque compiler
wrapper or cross-command recovery capability is established by this case.

The publication/restart replay also passed at that source in **111.88 seconds**,
including both negative credential-role cases before main. Original compiler
cold/warm `[3,0]`, build-script `[1,0]`, actual E0761/Cargo101 retry, publication
history, pre-activation invalidation, original cleanup and sixteen zero-miss
kernel programs/links remained verified. All **307 Python CI tests passed with
no skips**, and **31 focused Rust tests passed** with 17 opt-in cases excluded.
The repository and protocol VM cases were then explicitly run. Both adapter
modes remain disabled in production builds; the unchanged original in-command
regression, complete input closure and passing-CI/merge authority remain open.

Protected command recovery now uses a generation-owned execution checkpoint.
A claim becomes active before controller launch and binds the actual controller
scope. It closes only after that original scope has stopped and output
observation has finished. A later host-acknowledged command must supply the exact
predecessor; missing, active, stale or torn claims hold without allocating a new
cold executor. The protected root is pinned by identity. Retained planner/home
metadata, command records, sealed objects and producer history are checked
before reopening. Cargo's internal hardlink aliases are retained; external
aliases and unsupported entries refuse the checkpoint.

Publication recovery reconstructs active indexes from the immutable producer
records and checks original successful-exec/cleanup evidence, allocations and
sealed content/metadata. Old records retain their original command binding.
Incomplete allocations/transitions remain held. Each new command gets separate
view slots and retains historical command directories. Per-command execution
and restart limits are not reset inside the command by recovery.

Stores with generated namespaces or selector registrations still hold on
recovery: callback closures are not a persistent selector protocol. The actual
repository adapter test now includes another VM/command on the same source and
requires zero compiler calls for unchanged library and test units. Native
validation at `26cf97b3` passed in **115.32 seconds**: the first command retained
its original compiler `[1,0,1]`, credential, metadata-denial and cleanup criteria;
the next VM recovered the predecessor's protected state and completed build/test
with compiler counts **`[0,0]`**. The test executed again successfully. No cold
replacement target could meet those criteria. Immutable producer records kept
their original command/evidence while the store acquired the new command claim.

The publication/restart replay passed at the same source in **114.73 seconds**,
including crossed-role rejection, original compiler/build-script reuse,
E0761/Cargo101 retry, immutable history and the sixteen zero-miss programs/links.
The structured adapter success/failing-test replay also passed in **186.41
seconds**, retaining original argv and actual Cargo101/diagnostics. All **311
Python CI tests passed without skips**, and **31 focused Rust tests passed** with
17 opt-in cases excluded; these three VM cases were then explicitly run. This
ownership/continuation mechanism is not a complete-input or passing-CI
capability. Generated namespaces/selectors, frozen generated forests, opaque
counter-wrapper execution and the original in-command regression remain open.
Production activation remains disabled.

Declared generated forests now have a separate controller-owned immutable store.
A successful non-Cargo repository step publishes only after its original worker
scope and input observation have closed. Its record binds the original command,
claim, scope, declared namespaces, public metadata and sealed tree. A durable
completed-command ledger preserves that producer evidence across later claims.
Failed steps cannot publish new generated-input authority. Unchanged forests
retain the original producer; changed forests receive a new identity without
altering previous snapshots or their original file timestamps.

The actual Cargo namespace and compiler/application views receive read-only
binds of those exact sealed roots. A private manifest path carries the large
inventory outside the bounded sealed bootstrap. Recovered planner metadata also
binds the forest references it compiled against. Until persistent selector
revalidation is implemented, a changed forest refuses that retained compiled
state rather than running stale outputs or replacing the executor with a cold
one. This is an intermediate execution mechanism, not completed D8 or complete
input/provenance compatibility.

The new opt-in `live_shared_ci_generated_forest_execution` case exercises a
completed seed, real build/unchanged build/test, cross-VM unchanged reuse, a new
same-mtime producer and a typed hold before incompatible reuse. The existing
repository recovery and original in-command generated-publication tests retain
their criteria. Production modes remain disabled, and opaque wrapper execution, selector recovery, full input
closure and the final passing-CI/merge enforcement remain unfinished.

The first generated-forest native run held before publication: the actual source
projection has controller-owned 0555 parent directories, whereas the old output
copier required worker ownership on every ancestor. The retained stopped VM had
an allocated empty object and no published record. Copying now distinguishes
explicit frozen controller prefixes from worker-owned generated outputs; both
retain their own ownership/mode checks. Snapshot timestamp updates are fsynced
before publication. This failure did not grant input or shipping authority.


Native validation at `5ccaf6f8` passed in **145.63 seconds** across five actual
VM commands. The original successful seed published one generated forest after
worker cleanup. The actual repository driver compiled library/build/test with
counts **`[1,0,1]`**, and the following VM recovered both unchanged units with
counts **`[0,0]`** while executing the original test again. It retained the seed's
exact producer reference. A later completed producer changed the generated file
while preserving its timestamp, published a different reference, and left the
old sealed snapshot intact. The next Cargo attempt returned the expected typed
`InputCensusUnknown` hold before incompatible compiled output could pass. This
hold is an interim boundary, not completed selective revalidation or D8.

All **316 Python CI tests passed without skips**, and **31 focused managed Rust
tests passed** with 18 opt-in cases excluded; this new VM case was explicitly
executed. The original in-command counter/publication harness remains unchanged
and unfinished. Complete input closure, opaque wrapper execution, passing-CI
proof and merge enforcement still gate production activation.

Compatibility at the same executable source also passed: the publication/retry
replay completed in **111.84 seconds**, retaining original credential-role,
stdio, cleanup, compiler `[3,0]`, build-script `[1,0]`, actual E0761/Cargo101,
immutable-history and sixteen-program/link zero-miss criteria. The structured
success/failing-test replay completed in **192.13 seconds**. Both cases retained
compiler counts `[2,1]`, original build/test argv and original test output; their
actual test Cargo exits were 0 and 101 respectively. These are component
execution/recovery checks, not completed full CI or an autonomous merge.

Actual structured and repository adapters now start a general-path kernel
collector before admitting units. The original private fixture mode and its
path/diagnostic criteria remain separate. General captures preserve literal
runtime/relative/parent-component names rather than rewriting them; resolution
and compatible-input selection remain the index's responsibility. The same
original sixteen attachments and zero-miss census are required.

After original supervisor cleanup and relay drainage, each record binds unit
credentials, root/namespace identity, terminal supervisor evidence, request
identity and decoded kernel lookups/directory/metadata facts. Compiler units
must include an actual successful source open. Child-process observations stay
with their original unit credential; unknown unit credentials hold. Metadata
preparation units are retained separately from the original build/test commands.
Records are controller-owned and included in retained executor checkpoints.
The host validates the exact command, unit set, census and false authority flags.
This is actual-path collection groundwork, not full syscall/input/runtime/FD
coverage, compatible reuse authority, or passing-CI evidence.

The generated-forest replay now uses `arbitrary/source.rs` and requires original
compiler source opens plus unchanged-unit reuse, covering paths outside the old
fixture allowlist. Native validation with the general collector is recorded below.
Durable selector revalidation, opaque wrapper execution and the original
in-command publication regression remain unfinished; production is disabled.

The first general-capture native attempt completed collection and closed its
original execution scope, but host receipt validation refused it. A validator
loop reused the command-ID parameter name for each Cargo-command record; the new
collector binding check then compared against that record. The loop now keeps
the command ID intact, and the mocked transport regression includes a nonempty
Cargo/compiled-unit result. Read-only inspection of the stopped image retained
1,932 pathname observations, 32 directory reads and 554 metadata observations
across three information units, two compilers and one application, with unchanged
zero-miss kernel programs. The failed host validation granted no CI authority.

Native validation at `de36b714` passed in **162.50 seconds** across the original
five-command generated-forest sequence with general collection enabled. Both
compilers opened `arbitrary/source.rs` in the original kernel records; their
unit captures contained **417 and 1,341 pathname lookups**. Three information
units and one application remained separately attributed. The first driver
retained compiler counts `[1,0,1]`. The next VM retained the previous observations,
compiled neither library nor test (`[0,0]`), and executed the test again. The new
same-mtime generated producer still held before incompatible reuse. Capture and
recovery grant no complete-input or passing-CI authority.

The unchanged private publication/retry replay passed in **109.57 seconds**,
including actual E0761/Cargo101, original cleanup, selective retry and all sixteen
original zero-miss programs/links. The structured success/failing-test replay
passed in **192.50 seconds** with general captures, original argv, compiler
counts `[2,1]`, and actual test exits 0/101. All **319 Python CI tests passed
without skips**, and **31 focused managed Rust tests passed** with 18 opt-in
cases excluded; these three VM cases were explicitly executed. Durable selected
input revalidation, opaque producer/wrapper mediation, complete input closure,
the original in-command regression, final proof API/audit and production merge
enforcement remain unfinished. Production remains disabled.


A subsequent read-only inspection of the stopped image under the managed lease
verified both original observation-record SHA-256 values across recovery. The
first record retained 1,932 pathname queries, 32 directory reads and 554 metadata
observations, including the literal generated lookup
`arbitrary/../node_modules/generated.rs`. The recovered record retained 33
pathname queries, 10 directory reads and 48 metadata observations for the actual
application, with no invented compiler executions. The original raw names and
scope evidence are inputs to the next selector step; they are not normalized
into a compatibility or passing-CI verdict.

General read collection now binds kernel-copied `do_file_open` arguments and the
`fd_install` file object to the original syscall's returned descriptor. It keeps
open flags, read capability, inode/device/mode/generation, dentry/mount and
namespace identities. General mode requires **18** original zero-miss
programs/links; the unchanged private fixture keeps its original **16**. Missing,
duplicate, crossed-descriptor or namespace evidence refuses capture. Syscall
439 (`faccessat2`) cannot count as a source open; open/openat/openat2 are distinct.
Compiler source evidence requires a readable installed object matching the
actual frozen source inode, and excludes `O_PATH` handles.

The kernel argument/installation relationship follows the original open path
([Linux open.c](https://github.com/torvalds/linux/blob/v7.0/fs/open.c#L1276)); actual
field layout, function identity and attachment counters must still pass against
the pinned VM kernel. No caller-memory sample substitutes for the copied flags.
The internal general capture envelope is version 2; old captures cannot provide
the new read evidence.

Frozen indexes now support declared nested directory and file overlays while
still verifying each complete backing inventory. Hidden source placeholders
cannot stand in for generated input objects. Literal parent components remain
part of lookup resolution. A selected read binds original kernel bases, flags,
installed inode and namespace to frozen content. Its serialized queries and
content facts are rechecked against the original trees before refresh. Unknown
reads remain explicit, so the selected subset never claims complete coverage.

On a forest change the actual executor now rechecks selections for active
compiled producers and persists a `selected-revalidation` record before holding.
The record retains the original capture/command/unit binding, old/new content
digests and affected fingerprint names. It neither retires fingerprints nor
changes old snapshots/producer records, and unchanged selected facts do not grant
reuse authority. Complete input coverage and durable invalidation/restart remain
required before releasing that hold. Actual VM validation of this revision is
pending. The original wrapper/publication harness remains unchanged and
unfinished; production activation remains disabled.

The first native attempt of the installed-open extension held before collector
launch (**80.32 seconds** overall). The initial helper name was from an older
kernel; the pinned VM is Linux 7.0 and requires `do_file_open`. Its original
symbol census refused the missing symbol before creating collector output. The
probe and required census now use the pinned kernel's function. No attachment
requirement or miss check was relaxed.

The second native attempt attached both new probes and retained copied flags and
installed-descriptor evidence, but startup parsing still used the original
16-probe loader-summary bound (**84.84 seconds** overall). The parser now derives
that diagnostic bound from the required probe count. The original mode retains
its exact prior bound; general mode remains bounded and still requires all 18
original attachments and zero misses. The retained actual stream now decodes
successfully under the corrected bound.

Native validation at `070c0b21` passed in **162.06 seconds** with the full general
18-probe set. Original compiler counts `[1,0,1]` and cross-VM unchanged `[0,0]`
remained intact. The compiled source opens now require the actual readable
installed objects, not pathname/access observations alone. The original test
executed again, and the same-mtime generated change remained held.

Read-only inspection of that stopped image under the managed lease verified the
actual persisted revalidation report: **exactly two active compiler
fingerprints** had changed selected-content digests. Each retained its original
capture SHA-256, command, ordinal and binding. The report explicitly retained
**44 and 70 unbound read opens**, respectively, and kept complete-input/full-CI/
shipping flags false. It did not retire fingerprints or alter old input bytes.

Compatibility at the same source passed: the original private publication/retry
case completed in **111.30 seconds**, retaining its **16** original zero-miss
programs/links, real E0761/Cargo101, immutable history and cleanup criteria. The
structured success/failing-test case completed in **197.29 seconds** with **18**
original programs/links, original build/test argv, compiler counts `[2,1]` and
actual Cargo test exits 0/101. All **321 Python CI tests passed without skips**,
and **31 focused managed Rust tests passed** with 18 opt-in cases excluded; these
three VM cases were explicitly run. Complete input revalidation, actual selective
invalidation/restart, opaque wrapper/producer mediation, the unchanged original
in-command regression and final proof/merge enforcement remain unfinished.

Actual compiler captures now retain selected metadata lookup outcomes alongside
read-open selections. Missing-path facts are rechecked against frozen declared
source/generated overlays; unknown metadata, namespace/descriptor mismatches
and unsupported paths remain explicit. The original private metadata selector
uses the same binding checks. The new subset path verifies backing trees once
per batch and does not turn a selected subset into complete metadata coverage.

Internal kernel records are now version 3 because each compiler's persisted
metadata selection and unbound-query list are required during refresh. The
revalidation report (version 2) retains separate read and metadata relevance
changes and combines their digests for affected active producers. Old records
cannot substitute for the new metadata evidence. Neither changed nor unchanged
selected facts approve reuse or retire fingerprints.

A new opt-in `live_shared_ci_generated_negative_lookup_revalidation` replay uses
actual repository commands and compiler/test execution. Its producer adds a
conflicting `chosen.rs` at the old timestamp while leaving every previously read
generated file's bytes and timestamp unchanged. The positive-read replay and
original in-command wrapper/publication harness remain unchanged. The expected
native evidence is changed metadata/absence relevance with unchanged read
relevance, bound to the original active producers, followed by the mandatory
incomplete-input hold. Native results for this revision are pending.

Native validation at `a50724f1` passed the new missing-module replay in **160.85
seconds** across five actual VM commands. Original compiler `[1,0,1]` and
cross-VM unchanged `[0,0]` criteria remained intact, with the original test
executed again. The later producer introduced only `chosen.rs`; its assertions
verified all previously read generated bytes and old timestamps were unchanged.

Read-only stopped-image inspection under the managed lease then verified the
actual version-2 revalidation report: **both active compiler fingerprints kept
unchanged read relevance and acquired changed metadata relevance**, retaining
their original capture/command/unit bindings. This detects changed absence even
when positive input content does not change. Their 44/70 unbound read opens and
61/98 unbound metadata queries remain explicit, and all complete-input/full-CI/
shipping flags remain false. No fingerprint retirement or complete revalidation
is claimed. Existing positive-read and other compatibility replays for this
source remain pending.


### Failed opens and mounted-directory boundaries

The original collector already records failed `open`, `openat` and `openat2`
calls. Compiler selection now retains supported read-only `ENOENT`/`ENOTDIR`
outcomes using the same original flags, namespace, lookup base and result;
failures with an installed descriptor or an inconsistent census stay unbound.
These selectors survive protected recovery and refresh against the later
forest, so a newly present path can invalidate relevance even when all files
previously read retain their bytes and timestamps. Unsupported error/flag/path
combinations remain explicit, rather than disappearing before selection.

The actual compiler index also treats `/workspace/target` as an opaque mounted
subtree. Source placeholders beneath that mount cannot establish content or
absence in the mounted artifact view. Explicitly frozen descendant roots remain
selectable; unknown children hold. The opaque layout is part of the persisted
selector identity, so recovery cannot silently remove that boundary. Both read
and metadata selection use it. This is still incomplete input observation;
`input_closure_complete`, `full_ci` and `shipping_authority` remain false.

Regressions were run red before implementation: all three open syscalls dropped
both absent-file and non-directory outcomes; the actual command adapter omitted
the failed open from its persisted selection. New tests cover recovery, an
unrelated sibling, same-timestamp introduction, hidden source placeholders,
namespace/descriptor/result mismatch, and explicitly frozen descendants through
an opaque mount. An early new-test run correctly hit the frozen-tree atime guard
after its own ordinary byte read; the test now compares through the existing
no-atime inventory reader. The guard remains unchanged.

Compatibility at source `a50724f1`, evidence head `44a4d622`, completed before
these edits: original positive generated replay **162.33s**, original private
publication/retry replay **110.40s**, and original structured success/failing
replay **190.63s**, all passed. Receipts are under
`/tmp/augmentagent-ci-negative-positive-20261103192758`,
`/tmp/augmentagent-ci-negative-protocol-20261103193041`, and
`/tmp/augmentagent-ci-negative-structured-20261103193233` respectively.
These are compatibility evidence for that source, not results for later edits.


Validation for executable source `c0238838`: **325 Python CI tests passed with
no skips; 31 focused Rust tests passed**, with 19 opt-in native cases excluded
from routine execution. The actual five-command generated-negative replay was
explicitly run and passed in **159.68s**. It retained original compiler counts
`[1,0,1]`, recovered `[0,0]`, original test rerun, worker metadata/direct-exec
refusals and the later generated-change hold. Receipt:
`/tmp/augmentagent-ci-negative-open-20261103194202/receipt.json`.

Read-only inspection under the managed lease verified both active affected
compiler records against the original capture digest and command, including the
persisted opaque target boundary. Their read-content relevance remained equal
and metadata relevance changed. These particular successful compiler scopes
selected three opens each and no failed opens; failed-open behavior is covered
by the red/green selector and command-adapter regressions, not claimed as
native failed-open coverage. The scopes retain **77/105 unbound opens** and
**68/104 unbound metadata queries**. The hold and false authority flags remain.
Inspection log: `/tmp/autoship-1703-negative-open-revalidation-inspection-1.log`.

The normal production worker separately retried workflow #1796 at generation
71 and recorded `disk full`, failure count 27, with model calls still 58 and
all five merge slots available. A read-only inspection under that cache's
managed lease found 681,807,872 free filesystem bytes in its 12-GiB legacy
image after the attempt. Of that, 157,286 blocks are reserved: the unprivileged
worker has at most **37,564,416 bytes (about 36 MiB)** available. The image has
`/target` and `/cargo-home` allocations and no new generation layout. This does not prove ownership of its contents for
reclamation or implement migration. Bounded ownership-safe legacy recovery
remains a rollout gate; no cache capacity/accounting/backoff was changed.


### Bounded retirement of legacy incremental sessions

The production image's older `/target` allocation comes from the privileged
cache bootstrap. Its controller-owned image root protects creation/replacement
of that delegated target root. Cleanup ownership here is distinct from artifact
producer provenance: retiring a derived incremental session cannot certify any
compiled artifact or make an old CI receipt current.

The guest now has a pre-worker retirement path for recognized legacy Rust
incremental sessions. It runs only after the existing registered-image/target
admission and prior cleanup reconciliation, in the fresh guest before workers
start. Host admission additionally verifies the image is beneath the exact
physical target whose original inode lock is inherited and still held; an
extra unrelated descriptor does not establish ownership. RAM, cache capacity,
host disk floors, reservations and normal managed exclusion remain unchanged.

Retirement covers only the legacy `target/{debug,release}/incremental` namespace,
with matching original worker ownership/device, safe permissions, a held
nonblocking session lock, flat recognized compiler files, no symlinks, no
additional hard links and no extended attributes. Unknown entries retain the
entire session. Compiled artifacts, fingerprints, Cargo downloads, source/audit
evidence and new generation executors are outside this cleanup path.

Before moving a selected session, root commits a versioned intent under a
controller-private retirement allocation. The session is atomically moved into
that allocation, then removed. Recovery validates the exact original directory
chain and remaining file identities; changed/rebound state holds. A failed
initial intent cannot move the source. An empty private envelope can be reconciled
on retry; an envelope containing unknown data remains protected. Completed
records are compact historical evidence with false shipping authority. Physical
cache replacement fences pending retirement, while valid completed history
stays readable without becoming current authorization.

Each attempt acknowledges at most 32 sessions; discovery and per-session record
sizes are bounded, and completed histories are compacted under a fixed record
limit. Both Python transport layers and the Rust adapter validate the bounded,
unique retirement identifiers. The Rust audit records them with the current
binding, physical-cache identity and original quiescence receipt. These are
cleanup acknowledgements, never passing-CI capabilities.

Red/green tests cover interrupted moves/removals, ENOSPC before intent,
post-intent mutation, cache replacement, unknown files/links/permissions,
busy locks, bounded retirement and historical records. The pre-worker VM path
and actual warm Cargo reuse after retirement still require native validation;
filesystem tests alone do not close that gate. This code has not been applied
to the production image and autoship restoration remains incomplete.


A separate red/green admission regression found that a reopened lock pathname
could borrow the appearance of a lock held through another open description.
The adapter now checks the kernel's per-descriptor `fdinfo` flock ownership for
the inherited global lease and exact physical-target lease. True duplicated
inherited descriptions still pass; reopening `/proc/.../fd` does not. Independent
contention probes remain in place. This matches the existing Rust parent-lease
rule and does not acquire another build slot or weaken exclusion.

The managed Rust verification begun before that final admission refinement
ended with exit 75 while the normal worker had a live QEMU child. No live build
was killed. A duplicate queued verification (no build descendants) was cancelled;
verification of the latest helper bytes uses the same registered target and
normal admission. Native legacy retirement and compatible-unit reuse remain
unproven until their actual VM replay is run.


Validation at executable source `3cffbc52`: **31 focused managed Rust tests
passed**, with 19 opt-in cases excluded from routine execution. After updating
one existing unlocked-lease diagnostic assertion and checking that a retained
lease also refuses after unlock, **337 Python CI tests passed with no skips**.
The actual five-command adapter replay was explicitly run and passed in
**161.56s**, after verifying all nine embedded helper payloads against source.
It retained original compiler counts `[1,0,1]`, recovered `[0,0]`, original test
execution and the later generated-change hold. Receipt:
`/tmp/augmentagent-ci-legacy-incremental-20261103202518/receipt.json`.

The protected audit recorded one real legacy retirement during that replay.
Managed read-only inspection verified the compact completed receipt against
its original command and physical cache: **259 files, 556,438,022 logical
bytes**, under `target/debug/incremental/augmentagent-027wj69ei8meo`.
Artifact and fingerprint namespaces remained present, and the subsequent
current-generation compatible builds reused their units. This is not a
before/after byte proof for the older legacy compiled artifacts, nor a test
of their reuse as trusted artifacts. Audit extraction and inspection:
`/tmp/autoship-1703-native-legacy-retirement-audit-1.json` and
`/tmp/autoship-1703-native-legacy-retirement-inspection-1.log`.

Only this registered test image was changed. No recovery was applied to the
production image. Production capacity recovery, independent audit and the
remaining source/provenance/merge gates remain open; this evidence cannot
authorize an autonomous merge.


### Read-only recovery rehearsal on an existing image

The opt-in `live_shared_ci_legacy_recovery_preview` exercises recovery against
an existing registered image without changing that image. It uses the ordinary
managed CI admission, inherited global and exact target leases, original VM
resource policy, cleanup hold and verified descendant teardown. A small Git
repository identifies the diagnostic context; it is not application CI.

The dedicated preview controller uses the pinned QEMU runtime with per-drive
`snapshot=on`, which redirects writes to temporary storage ([QEMU invocation
reference](https://www.qemu.org/docs/master/system/qemu-manpage.html)). QEMU starts
with its virtual CPUs paused. Before `cont`, the controller verifies actual
kernel descriptors for the base image are read-only and identifies the writable
temporary overlay in registered scratch storage. The ordinary human monitor is
disabled; QMP is confined to an owner-private runtime socket. The result records
this temporary-view lineage, rather than claiming it is an ordinary physical
cache or passing-CI proof.

Inside that view, the trusted guest inventories and hashes all protected cache
namespaces before and after real legacy incremental retirement. Only the legacy
incremental and retirement-metadata namespaces are excluded. File bytes,
identities, modes, ownership, regular-file links/timestamps, symlink targets and extended attributes remain
part of the protected inventory; directory times and link counts are excluded
because namespace retirement legitimately changes them. Hard-linked contents
are hashed once with each path still represented. Entry/byte bounds and the
existing command deadline fence unsupported inventories.

The controller hashes the entire original raw image before and after the
rehearsal and verifies its identity. Read-only cache validation cannot format
or create a missing image. Unknown descendants retain control state under the
existing cleanup-hold rules. The preview reports hypothetical worker-available
space and cleanup acknowledgements, with completeness, full-CI and shipping
authority false. It does not execute application tests, validate legacy
artifacts for reuse, or advance an autoship workflow.

Unit regressions cover missing-image refusal, snapshot/paused launch policy,
real kernel read-only versus writable descriptor checks, protected artifact
changes, no-atime source hashing and malformed/authority-bearing outcomes.
The actual production-image rehearsal at executable `6bf753bc` passed in
158.46 seconds, with ten embedded helper payloads verified. Its temporary view
retired four sessions and increased worker-available space from 20,738,048 to
53,387,264 bytes. The protected inventory remained identical: 45,834 files and
4,167,370,136 unique logical content bytes. The original raw image's full SHA-256
and recorded identity also remained identical. Receipt:
`/tmp/augmentagent-ci-recovery-preview-20261103210041/receipt.json`.

This is an insufficient capacity result, not production recovery. The strict
single-link policy leaves shared incremental objects retained. An independently
admitted read-only census found 192 matching sessions, including sampled object
link counts of 2 and 52; those samples do not establish a complete safe link
closure or a physical reclaim estimate. A bounded closed-link policy still
requires implementation, fault validation and independent review. The latest
routine validation passed 343 Python CI tests without skips and 31 focused Rust
tests, with 20 opt-in native cases excluded from that routine Rust run. The
preview was explicitly executed separately. Original source/proof/merge gates
remain open, and no production cache change or autonomous merge is claimed.

### Closed-link recovery under validation

The opt-in cleanup policy now censuses recognized sessions within one legacy
incremental unit while retaining their actual lock descriptions. A linked file
is eligible only if every inode alias is accounted for in safe locked sessions.
Foreign aliases, unknown/busy peers, duplicate selected-file inodes and oversized
censuses remain retained. One session is retired per ID, with the original
32-session limit; the census is refreshed after each retirement. Version-2
cleanup intent records peer directory/lock identities and indexed endpoints;
partial deletion drops obligations only for selected files already removed.
Version-1 selection remains the default and both known record versions replay.
The ordinary pre-worker caller still selects that default during validation.

Local validation passes 359 Python CI tests without skips and 31 focused managed
Rust tests, with 20 opt-in native cases excluded. This includes real filesystem
links, 41 linked sessions across the 32-session attempt boundary, intent and
partial/completed-deletion interruptions, foreign/busy/changed peers, malformed
records and strict-selection lock lifetime. The native comparison is pending:
under one existing storage lease it runs two separately supervised commands,
each with the original 180-second deadline, in fresh temporary views of the
same base. It requires identical base/protected inventories and initial worker
capacity, and a larger final capacity with closed-link cleanup. Neither command
runs application CI or grants reuse/merge authority. The proposed contract is
[ci-legacy-hardlink-recovery.md](ci-legacy-hardlink-recovery.md); its independent
review is incomplete and production activation remains gated.

The first same-base comparison completed its strict reference in the real VM
but the second diagnostic command hit `EEXIST` before launch: a Session reused
fixed private request/result names. They now include the original command UUID.
The successful reference again retired four sessions, increasing available
bytes from 20,713,472 to 53,362,688; its base was unchanged. No closed-link native
capacity result was produced by that attempt.

Fault validation now covers an abrupt process exit inside the atomic writer,
before `receipt.json` publication. Only a bounded, correctly owned mode-0600
single-link writer temporary in the protected mode-0700 envelope is discarded;
unknown data remains held. An allowed-format synthetic backlog of 33 pending
intents now fences worker launch after the 32-session replay budget; a later
admission can finish the remaining intent. This is a format-safety regression,
not evidence that production naturally accumulated that backlog. These two
regressions fail against `e30b69a8` and pass after the changes. Routine validation
now passes **361 Python tests without skips and 31 focused managed Rust tests**
(20 native cases excluded). The native comparison requires a rerun.

All 57 mapped amendment seams were checked with fresh leaf reviewers. Findings
at six seams produced exact compact-hash/count definitions, private-writer crash
recovery, bounded pending-drain fencing and precise inventory field/exclusion
definitions. The two retained-scope reporting findings remain one explicit
R-owned blocking handoff; the ID-only normal result does not yet explain skipped
scopes/bounds. No production activation or completed final C audit is claimed.

The rerun at exact executable `c88829e3` passed the real same-base comparison in
**254.30 seconds total**, as two separately supervised commands under one original
storage admission and their unchanged individual deadlines. Both fresh views
started at **20,660,224 available bytes**. Strict cleanup retired four sessions
and ended at **53,309,440 bytes**; closed-link cleanup retired 32 sessions and
ended at **2,124,406,784 bytes** (about 1.98 GiB). The original attempt limit was
not raised. Both protected before/after inventories were identical to each other:
**46,346 files / 4,167,370,136 unique logical content bytes**. The entire original
raw image's identity/SHA stayed identical across both commands. Kernel backing
descriptors were verified read-only before each guest continued; retirement IDs
were unique and each result retained false completeness/full-CI/shipping flags.

Receipt: `/tmp/augmentagent-ci-recovery-comparison-20261103215444/receipt.json`,
SHA-256 `16bde525eaa0a5e2ec51751bd25554ea385fe0aeb3cdc7dd73e2c0939c8c176a`.
This establishes the closed-link mechanism's measured capacity benefit and
protected-inventory preservation on the production base. It does **not** apply
recovery to production, validate legacy compiled units as current inputs, prove
the intended application check plan fits, or authorize a merge. The retained-
scope reporting carrier and ordinary pre-worker opt-in remain unfinished;
source/proof/merge enforcement and the normal rollout/acceptance gates remain.

### Recovery reporting carrier and worker integration

The normal pre-worker caller now explicitly selects the closed-link policy.
Cleanup returns its original bounded retirement IDs and fills a cleanup-only
diagnostic in the same trusted result. The report binds the original command and
physical cache; it counts inspected/retired/replayed/retained selections, fixed
retention categories, encountered unknown/unsafe namespaces and reached bounds.
It neither estimates total retained capacity nor grants reuse/CI authority.
Planning without a generation/cache carries an explicit null diagnostic.

The existing VM shape and Python adapter carry the report; Python and Rust
independently reject foreign command/cache bindings, unknown fields, booleans as
counts, inconsistent accounting, unknown/duplicate bounds and shipping authority.
After verified command quiescence, Rust records it through the existing protected
controller audit with the source binding, physical cache and original teardown
receipt. The preview uses the same report with its explicitly chosen reference
or closed-link policy. No new cleanup journal, allocator or shipping proof exists.

Review also closed two reporting blind spots: a full *valid completed* history
retains new selections and reports `history_records`, while malformed/oversized
history or unfinished replay still hold; unknown/unsafe namespace observations
trigger a separate admitted diagnosis without following or deleting those data.
Removed unpublished envelopes release their history slots in the same attempt,
and completed replay does not consume a second slot for an existing record.
Real-filesystem red/green tests cover both slot calculations and report limits.
The updated independent review of S26/S42/S46 found no remaining quoted seam
defects at source/test-review level. Routine validation for this carrier revision
passes **370 Python CI tests without skips and 32 focused managed Rust tests**
(20 opt-in native cases excluded). The separate VM shape suite passes 14 cases
with six VM/runtime cases excluded. The subsequent native runs are recorded
below; the earlier capacity result at `c88829e3` is not presented as a run of this
later executable. Production remains
unchanged; final C proof, real-plan sufficiency, M enforcement and rollout gates
remain open.

At exact executable `1eb2b5e5`, the ordinary five-command generated-input adapter
replay passed in **169.65 seconds**. Its existing protected audit contains four
cleanup diagnostics, each matching its original command/cache and a protected
`shared_ci_vm_quiescent` record for the same command/source binding. The first
successful command retired five existing sessions and retained 44 with
`unclosed_links`; compatible cross-VM compiler reuse, generated-change hold,
metadata refusals and raw-worker exec denial remained intact. Audit extraction:
`/tmp/autoship-1703-legacy-diagnostic-native-audit-1.json`.

The same executable's production-base comparison then passed in **250.71
seconds**, under the original admission and individual command deadlines. Both
views began at **20,635,648 available bytes**. Strict selection inspected 192
sessions, retired four, retained 188 and ended at **53,284,864 bytes**. Closed-link
selection inspected 79, retired 32, retained 47 (45 unclosed-link and two unsafe
sessions), reported `attempt_retirements`, and ended at **2,124,382,208 bytes**.
The exact same protected inventory—**46,602 files / 4,167,370,136 unique logical
content bytes**—and entire original raw-image identity/SHA were preserved across
both views. The report is a bounded observed selection scope; it does not claim
the remaining sessions were inspected or recoverable.

Receipt: `/tmp/augmentagent-ci-recovery-comparison-20261103222854/receipt.json`,
SHA-256 `656c3a7c66d687a3a7f727a511bbca5b32024146c2f83fc4c7a7120df79a68ac`.
The reporting carrier and explicit worker opt-in are now native-validated.
Production is unchanged, the actual required application-plan sufficiency gate
remains open, and no input-completeness, full-CI or shipping authority is granted.

### Compiler artifact input bindings

Compiler read/metadata selection now includes the exact immutable artifact mounts
used by the original unit. The controller reads its original root-private mount
manifest and validates command, source binding, ordinal and declared outputs.
Referenced libraries and generated trees must belong to retained successful unit
publications with original execution/cleanup evidence; matching root-owned bytes
or a path outside that history are insufficient. The target subtree remains
opaque around the explicitly mounted leaves/trees, so an unknown sibling cannot
be recorded as proven absent.

The raw version-3 kernel record persists `artifact_bindings` inside each scope.
Older records without this field retain an empty artifact selection and their
unbound observations; they are not upgraded to complete input evidence. Restoring
an original selection authenticates those original inputs against immutable
history. Refresh resolves the same virtual paths through current active retained
producers, allowing changed library/tree contents to change selected relevance
without rewriting original captures or modifying prior artifact bytes. This is
input-observation plumbing, not an artifact-reuse or passing-CI capability.

Real-filesystem regressions cover mounted-file reads, opaque unknown siblings,
replacement artifact relevance, retained generated-tree replacement, forged
paths/aliases/digests, mismatched/malformed mount headers, and failed/diagnostic
producer evidence. The capture/recovery regression also uses an artifact from a
retained predecessor command. **374 Python CI tests pass without skips and 32
focused managed Rust tests pass** (20 native cases excluded). The actual command
adapter replay passed in **192.49 seconds** using the unchanged two-package
structured build/test fixture. Read-only inspection under the original managed
lease verified two actual compiler reads (`.rmeta` and `.rlib`) against one
retained successful producer and its original command capture/cleanup. Both the
passing build/test and failing test exits were preserved. Receipt:
`/tmp/augmentagent-ci-artifact-structured-20261103231131/receipt.json`; inspection:
`/tmp/autoship-1703-artifact-input-bindings-structured-inspection-1.json`. The
original inspector used the wrong guest cache alias; correcting it to the actual
controller-only `/root/managed-ci-cache` bind preserved every producer assertion.
The single-library negative replay is compatibility evidence only because it
has no dependency artifact mounts. Toolchain/runtime/directory/FD and
other unsupported inputs, the unchanged original opaque-wrapper/in-command
publication regression, selective invalidation/restart and final C proof/API/audit
remain open. All completeness/full-CI/shipping flags stay false.


### Original directory membership selectors

The original `iterate_dir` record now carries the executing task's mount and
user namespace identities alongside its inode, mode and starting cursor. Older
four-field identities remain decodable with explicitly absent namespaces; they
cannot acquire new directory-input authority.

The existing frozen-input index selects only a complete original `getdents64`
scan of a known immutable directory: original process and unit UID, original
namespace identities, exact directory object, continuous cursor from zero to
EOF, accepted byte counts, unique names and matching visible child inode/types.
Short reads are supported when they form that complete chain. Interrupted,
rewound, foreign, unknown-type or incomplete scans remain explicit unbound calls.
A directory containing an opaque mounted child cannot prove membership from a
hidden source placeholder. Multiple descriptor objects are reconciled separately;
repeated scans of the same descriptor/object are conservatively unresolved.

Selections persist in the original command record with the raw directory calls.
Refresh compares logical membership and includes it in selected-input relevance
and aggregate digests. An older capture without the field has unknown directory
coverage, not zero unbound calls. This is membership relevance only: inode and
cookie equivalence, returned ordering, other metadata and complete input closure
remain separate requirements. No selector enables reuse or passing CI.

The unchanged native unit's existing full scans, short reads, empty directory
and rewind provide the VM gate. The additional controller assertions require
real namespace-bound directory selections, retain the original rewind as
unbound, restore the original selection, preserve relevance after an unread
file's contents change, and detect an added member in the observed empty
directory. The original tool program, argv, streams, counters and budgets are
unchanged. **376 Python CI tests pass without skips.** The 32 focused managed Rust tests also pass (20 native opt-ins excluded).
The actual native protocol gate at `68288eef` then passed in **116.12 seconds**.
It selected six original directory calls, retained 29 unbound calls (including
the original three-call rewind), restored the original selection and detected
added membership while unread file-content changes kept the same identity.
The collector retained 558 pathname queries and 153 directory calls; the original
compiler counts stayed `[3, 0]`, and the invalidated library's retry still failed
with Cargo exit 101. Existing original-unit cleanup, source/metadata isolation,
signals, jobserver and exact probe-census assertions all passed.

Receipt: `/tmp/augmentagent-ci-directory-selection-20261103233113/receipt.json`,
SHA-256 `f4b2a19f776e52f1264d0959966e1fcf5de9bf9c7cd70ac754f7ed7080d134b6`.
This is actual directory-selector evidence, not a complete compiler-input proof.
All complete-input/full-CI/shipping flags remain false and production remains
disabled. Runtime/toolchain, direct descriptor and full metadata inputs,
publication-time invalidation/restart, trusted dependency seed integration,
the unchanged opaque-wrapper/in-command fixture and final proof/API/audit and
merge enforcement remain required.


The original five-command generated-negative adapter replay at `0ba63f7a`
passed in **164.29 seconds**. Read-only inspection under its managed lease
verified the original record SHA, new per-call namespace identities, persisted
selection fields and directory relevance fields in the durable changed-input
report. Unchanged cross-VM compiler calls remained zero and the changed generated
input still held. Its two compiler scopes each recorded ten actual directory
calls, all unbound; this adapter establishes capture/recovery integration rather
than positive directory-selector coverage. The preceding native protocol gate
provides that positive coverage. Receipt:
`/tmp/augmentagent-ci-directory-adapter-20261103233628/receipt.json`; inspection:
`/tmp/autoship-1703-directory-adapter-inspection-1.json`.

A further real-filesystem revalidation regression isolates a directory-only
change: reads and pathname-metadata selections remain unchanged while an added
generated member changes the aggregate digest, marks the consumer fingerprint
and persists the mandatory hold. Running it against the original `bf7c3842`
revalidation function fails because the affected set is empty. The current
function passes and preserves unknown directory coverage for older captures.
The Python suite now has **377 passing tests without skips**; executable helpers
are unchanged from the two successful native runs.

Inspection also identified the remaining actual read classes rather than
inferring completeness from the source-only subset: toolchain libraries and
metadata, loader searches through opaque target directories, generated compiler
scratch outputs, `/proc/self/maps`, `/proc/self/statm`, the VM overcommit setting
and rejected `/dev/fd` opens. These require an explicit trusted runtime/output/
descriptor policy. They are still unbound and grant no compatibility or shipping
authority.


### Original descriptor metadata selections

Successful original `fstat` calls now select their delivered x86-64 newstat
fields against the exact immutable input object. The existing kernel record
adds the executing task's mount namespace to its user namespace and descriptor
identity. Older records retain an explicitly absent mount namespace; they cannot
be selected as newly bound descriptor evidence.

The frozen index records each visible file/directory's exact mode, inode,
internal device identity, link count, owner/group, special-device value, size,
block size/count and nanosecond timestamps. A descriptor call must belong to the
original supervised process and unit UID, match both original namespaces,
report a successful VFS lookup and successful syscall delivery, and match every
one of those fields. Ambiguous input aliases, unknown objects, invalid or failed
descriptors, failed copies and changed metadata remain unbound. The kernel-only
`result_mask` is preserved in the raw record but excluded from delivered newstat
relevance. Descriptor queries do not select file bytes merely because they
queried metadata.

Original calls and selections persist together in the command capture. Restore
rechecks the original metadata against the original retained trees; refresh uses
the corresponding current visible inputs and contributes exact descriptor
metadata relevance to the existing aggregate digest and affected-fingerprint
report. A replacement inode with the same bytes is therefore a relevant metadata
change. This does not establish a metadata-normalization policy or permission to
reuse across differing delivered fields. Older captures have unknown descriptor
coverage, rather than zero unresolved descriptor calls. All refresh results keep
the mandatory incomplete-input hold.

The real-filesystem regression fails against `174c2b7d` because a descriptor-only
change leaves the affected set empty; it passes with the current implementation
and preserves the hold and legacy unknown-coverage state. Refusal tests change
every delivered field, credentials, namespaces, descriptor numbers and copy
outcomes, and cover duplicate call identities and ambiguous aliases. The existing
native probe has additional assertions for its unchanged binary-name file and
directory descriptor calls, retained EFAULT/EBADF calls, selection restoration
and changed replacement metadata. Its original executable body, argv, streams,
counters, cleanup and resource limits are unchanged.

**380 Python CI tests pass without skips and 32 focused managed Rust tests
pass** (20 native opt-ins excluded). The actual native results for this executable revision are recorded below. Full runtime/toolchain/output and
metadata input coverage, opaque producers and bounded publication restart,
trusted dependency reuse, final C proof/API/audit and consumer/merge enforcement
remain open. Complete-input/full-CI/shipping flags remain false.


At executable `bf22a3a9`, the unchanged native protocol fixture passed in
**116.64 seconds**. Six actual descriptor calls matched all delivered metadata
fields and their original namespaces; 114 failed/unknown calls remained unbound.
Selection restoration and replacement-metadata relevance passed. Original
compiler counts `[3, 0]`, invalidated retry exit 101, six directory selections,
29 unbound directory calls, signals, jobserver, metadata isolation and cleanup
assertions all remained intact. Receipt:
`/tmp/augmentagent-ci-descriptor-selection-20261103235935/receipt.json`, SHA-256
`cf8d797120fd24c60165e2a89e0b22d3767e5b511112ee7dd521d33e6fccf248`.

The original five-command adapter replay passed in **192.82 seconds** under the
unchanged per-command deadlines and resource policy. Managed read-only inspection
verified the original capture SHA, new descriptor namespace fields, selection/
unbound-call accounting and durable descriptor relevance fields. Its compiler
scopes had 20 and 22 actual fstat calls, all unbound. This establishes adapter
capture/recovery compatibility; the preceding native protocol gate establishes
positive descriptor selection. Unchanged cross-VM compiler counts remained zero
and the generated change held. Receipt:
`/tmp/augmentagent-ci-descriptor-adapter-20261104000151/receipt.json`; inspection:
`/tmp/autoship-1703-descriptor-adapter-inspection-1.json`. Both native runs retain
false complete-input/full-CI/shipping flags and zero autonomous merges claimed.

Hosted CI exposed two additional integration defects. The generated-forest host
fixtures assumed guest worker UID 1000 rather than the host fixture's own UID.
`GeneratedForests` keeps the fixed production worker role at UID 1000 through a
private class constant; host fixtures explicitly map that role to their temporary
filesystem owner. Real foreign-owned directory and single-file outputs still
refuse publication. The local Python suite now passes **381 tests without skips**.
The hosted bridge failure is recorded in job 126287631330 of run 41742617176.

The updater's embed-drift regression also reproduced a missing rebuild trigger
for `ci-legacy-recovery-preview.py`: **99 passed / one failed**. The release
rebuild pattern now includes that embedded helper. The same updater regression is
now green: **100 passed / zero failed**. Managed Rust validation passed 32 tests
with 20 opt-in native tests ignored. The unchanged five-command guest replay at
`51d6071a` passed in **165.02 seconds**; receipt:
`/tmp/augmentagent-ci-forest-owner-20261104003652/receipt.json`, SHA-256
`61023961e87f0e946929a996dd660ec07d8d8bd419e258641e761cba938a00c9`.
Hosted bridge and PDF checks also passed at that head. These repairs change no
live configuration or production cache.

### Namespace and asynchronous-I/O policy prerequisite

The constructor-free native helper now exports `jarvis_ci_restrict_unit`.
Controller bootstraps load its measured root-owned, single-link inode before
changing root, and activate it after trusted namespace setup but before dropping
credentials and entering worker, Cargo or unit code. It refuses namespace and
mount changes, namespace clone flags, io_uring and Linux native AIO. `clone3`
returns ENOSYS so ordinary libc thread/fork creation can fall back to inspected
`clone` flags. The policy synchronizes existing threads, survives fork/exec and
rejects foreign/x32 syscall routes. Existing exec denial remains separate.

The actual supervised kernel tests first failed against the old native helper
with seven missing-symbol failures; all ten kernel/loader tests then passed.
They exercise namespace/mount calls, both asynchronous-I/O interfaces, clone
flags, pre-existing threads and ordinary fork/exec/thread behavior. The complete
Python CI suite passes **391 tests without skips**, and the focused managed Rust
suite passes **33 tests** with 21 opt-in guest tests ignored. The separate
pinned-guest policy test at `7371d137` passed all seven kernel cases in **87.25
seconds**. Receipt:
`/tmp/augmentagent-ci-unit-policy-20261104005747/receipt.json`, SHA-256
`0ff3269fc6a2037f0f5a504afb85a671ed92e889c557acd5afe1819c65409d1b`.
The unchanged five-command generated-input adapter replay passed in **163.84
seconds**, preserving cross-VM zero compiler calls for unchanged inputs,
generated-change holds and original supervision/cleanup. Receipt:
`/tmp/augmentagent-ci-unit-policy-adapter-20261104010000/receipt.json`, SHA-256
`47944a4c88f04b349c383ffd2e4a2f1c87aace821cadd5553b510484824d8230`.
Both runs used existing managed admission and unchanged resource/deadline policy.
This establishes neither first-instruction exec mediation nor a complete input/pause proof. The
original generated-publication regression and all final proof, reuse and rollout
gates remain required; experimental authority flags remain false.

### First-exec mediation feasibility

The native helper now has explicit notification-listener/receive/response
primitives. The feasibility harness creates the listener in a trusted
single-threaded bootstrap before activating the unit policy. That policy also
refuses subsequent NEW_LISTENER creation, preventing a replacement notification
listener from overriding the original filter. Existing stricter relay exec
denial may still be installed.

The unchanged original supervisor observes the kernel exec stop. Its callback
closes initial-exec authorization before detaching, so no application instruction
runs while the initial permission remains open. Before that acknowledgement,
only the original live bootstrap PID can receive CONTINUE; this is not pointer
inspection or authorization of untrusted requests. Failed initial PATH
candidates may continue in that trusted bootstrap until a real exec succeeds.
All later raw exec notifications receive EPERM; loss of the listener returns
ENOSYS. Withdrawn notifications and terminal listener hangup are distinct from
received requests and never supply authorization or fabricated cleanup, following
the [kernel notification contract](https://docs.kernel.org/userspace-api/seccomp_filter.html).

The first five real syscall tests failed against the earlier missing API.
Six host cases now pass: early-constructor and body execve/execveat, an actual
static ELF without PT_INTERP, failed initial lookup, cancellation without an
exec acknowledgement, listener loss, and inherited mediation in a forked
descendant. Every case retains the original supervisor's descendant cleanup.
The complete Python suite passes **397 tests without skips**; the focused
managed Rust suite passes **34 tests**, with 22 opt-in guest tests ignored.
The separate pinned-guest gate at `627ca7f0` passed all six cases in **89.96
seconds** under the existing managed admission and unchanged command deadline
and resources. The receipt retains actual initial/denied notification records:
early constructor and fork each had four denied calls after acknowledgement;
static entry had two; failed lookup had two initial attempts followed by two
denials; cancellation had no acknowledgement; closed-listener behavior remained
fail-closed. All six retained original descendant cleanup. Receipt:
`/tmp/augmentagent-ci-exec-mediation-20261104011422/receipt.json`, SHA-256
`16b4ca3e30a4c7b7475216a19abd528bf6b12f7f5efa24039b91e5b2a70b04fc`.
Protected worker/Cargo/unit bootstrap integration is now implemented as described
below; its original protocol/adapter guest validation remains pending. These
primitives and experiments grant no passing-CI or merge authority.

The shared `ExecMediation` controller owns a separate root-local socket and the
listener FD under each original supervisor. Sealed bootstrap data binds the
transport FD; kernel message credentials and the original pidfd bind the sender.
The extra descriptor stays within the original FD/resource limits and is closed
before untrusted entry. Namespace setup precedes listener installation; the unit
restriction follows it. The original supervisor closes initial authorization at
its kernel exec callback before the existing acknowledgement and detach. The V3
control frames and original jobserver/stdio requests are unchanged.

Actual controller tests reproduce four failures against the preceding controller
source and now pass five cases using real notifications and the original
supervisor. Unsupported constructor, static-program and descendant exec attempts
cause a typed `input_census_unknown` controller hold, including a failed initial
PATH candidate followed by the original launch. Cancelled staging produces no
exec acknowledgement and retains original cleanup. The complete Python suite
passes **402 tests without skips**; the focused managed Rust suite passes **34
tests**, including the six feasibility cases and five controller cases, with 22
opt-in guest tests ignored. This does not yet establish the final C proof or
all-caller hold projection, and it does not enable production autoship.

The unchanged original V3 protocol replay at `29a895ca` is **red**: it reached
Cargo after the five original protocol scenarios and returned a prerequisite at
`planner_request_cleanup` after **95.14 seconds**. Retained image inspection was
read-only under the managed storage lease. No protocol fixture, counter,
acceptance condition or resource grant was changed. Integration acceptance and
the adapter replay remain blocked on this failure; it is not a passing native
result. A bounded root-owned per-view exec-mediation observation now records the
original acknowledgement, raw return status and notification counts to locate
the failure, without granting publication/reuse/shipping authority.

The second unchanged replay at `eb62e119` reproduced the hold in **95.77
seconds**. Managed read-only inspection found one initial authorization, no
denials, no notification errors and exit zero in each of units 5–7. Unit 8 had
one initial authorization, one denied subsequent exec, no notification errors
and exit one. Its retained output contains two real build-script compiler
`.rcgu.o` objects and dep-info, with no linked executable. This is consistent
with a denied linker launch; the exact requested executable was not captured.
The required repair is mediated nested execution, not raw helper authorization.
Observations: `/tmp/autoship-1703-exec-controller-inspection-4.json` and
`/tmp/autoship-1703-exec-controller-inspection-5.json`. Both failing replays
retained verified VM teardown and false shipping authority; no production
workflow, original regression or grant was changed.

### Scoped unit credential admission

The V3 relay now recognizes the distinct sealed `JARVUNT3` unit role. Its existing
uid field names the assigned client in 2000–6095; all real/effective/saved UID/GID
values must match, supplementary groups remain empty and the grant must be a
root-owned sealed descriptor. Worker/planner roles keep their existing uid=0
layout. The expected V3 server UID remains root independently of the unit's
client UID; V2's same-UID server behavior remains intact.

The actual pinned guest first reproduced rejection of a valid UID2000 unit grant
at `fd854de0`: constructor exit 125 before main, **82.57 seconds** for the replay.
After the role change at `03335b00`, retained root observations verify all ten
constructor cases: both boundary UIDs accepted, and eight wrong/out-of-range or
cross-role grants refused before main. These credential-only cases run outside
the original unit observer's UID/process-attribution window. **402 Python tests
and 34 focused Rust tests pass**, with 22 guest opt-ins ignored routinely.
Red observation SHA-256:
`c25b7f8777eaa006a647d5774ca02f0c2474dce7e0ced72363a74283bb2145dd`;
accepted/refused observation SHA-256:
`3ee1327caf2437f267e0c8cbb8d2dfbb6b4d7f5f17e41fc76b08d44af66c44db`.

This proves constructor admission, not unit-peer connection, contained dispatch
or output provenance. The unchanged full native replay remains **red** after
**94.27 seconds** at the known compiler subsequent-exec refusal. The first attempt
was blocked by deployment admission for 300.26 seconds and supplied no guest-test
evidence; it is not counted as the reproduced constructor failure. No production
configuration or shipping authority changed.

The [nested-producer mechanism draft](ci-opaque-producers.md) now records the
provisional-result/reuse distinction, worker liveness, artifact export slots,
inode/rename semantics and C/R recovery obligations from the 84-seam review.
Signal ingress, sibling-effect closure and complete kernel-policy disposition
remain explicit C-owned handoff blockers; the draft is not a final C proof/API.

### Original exec-entry observation

Before releasing the original kernel exec stop, the unit supervisor now samples
the actual PID/birth, tracer, single-thread census, four UID/GID slots,
supplementary groups, capabilities, no-new-privileges, seccomp status and bounded
descriptor table. It requires the assigned unit identity, empty groups, zero
effective/permitted/ambient capabilities and exactly the intended standard
streams, normalized jobserver ends and relay descriptor. Standard output/error
identity and access mode must match the original supervisor; both jobserver ends
must match the staged pipe identity. The observation is bound to the original
command and unit ordinal and cannot authorize transfer, reuse or shipping.

Three real host-process tests use the original supervisor and actual native unit
policy: inspection precedes the body's first instruction, an inherited extra FD
prevents body execution, and a running controller cannot substitute for the
original stopped tracee. Host tests observe the host's actual supplementary
groups; production requires empty groups. These tests do not establish private
guest namespaces, exact installed filter identity, complete writer isolation or
end-to-end autoship.

The unchanged four-parent native replay at `0c6543ae` remains red after 105.55
seconds. Managed read-only inspection verifies 16 contained exec-entry records,
four nested root children making progress with the original slots and jobserver
pipe exhausted, four successful deepest-child returns, and original resource
cleanup. Parent transfer/resume remains held and all CI/shipping authority flags
remain false. Observation: `/tmp/autoship-1703-four-parent-observation-3.json`.

The entry check additionally hashes the actual sealed stdin descriptor and
compares it to the original staging observation. An inherited relay config must
match the original supervisor's sealed inode, size, seals and content hash.
Unsealed and oversized descriptors hold before any content read; positioned
reads through an O_NOATIME handle preserve offsets, flags and timestamps. These
content observations still do not establish exact installed filter identity or
complete writer isolation.

The replay at `b12995aa` verifies sealed stdin/config observations in all 16
contained exec entries, with four successful deepest-child returns, four nested
root children progressing and original cleanup verified. The guest stdin was
empty; each relay config was 160 bytes. Nonempty content preservation is covered
by the host descriptor tests, not this guest fixture. The full native test stays
red at the held transfer/resume path after 104.66 seconds. The complete host
Python suite passes 575 tests; the managed focused Rust suite passes 34 tests
with 24 opt-in guest tests ignored in that routine run. Guest observation:
`/tmp/autoship-1703-four-parent-observation-4.json`. This does not enable transfer,
resume, passing-CI proof or production autoship.

The trusted unit bootstrap now measures the held native policy inode before and
after loading it. Changing its contents during loading holds; replacing its
pathname cannot substitute a different inode into that measurement. Only a
successful native activation can produce the bootstrap's activation observation.
An empty supervisor-created memfd carries the bounded receipt, seals before the
credential drop, and closes at exec. The original exec observer checks its full
command/ordinal/PID binding and the independently loaded policy digest.

Before enrollment, Root re-observes the same stopped process against the original
supervisor PID from its own launch handle, compares the complete recorded entry,
and verifies the policy receipt against its own measured runtime library. The
freezer retains that entry and checks each frozen thread's UID/GID, empty groups,
capabilities, no-new-privileges and seccomp state, including original PID birth.
The pause record binds the entry digest and policy digest. This records the
trusted installation path; it does not dump the installed kernel BPF program or
establish complete private-W writer/alias isolation, input closure or CI authority.

The native replay at `615887c5` verifies 16 policy-bound exec entries and 16
pause records against the compiled policy digest
`d1836f15fcaa81c98e86de7ac2987b87c30639b5f329704851b5f220b3e3b795`.
Twelve pause digests independently join retained parent entry records; the four
initial parent entry files are outside this retained executor traversal. All
four nested root children progress with the original pipe/slots exhausted, and
all four deepest children return zero with original cleanup verified. The full
test remains red at held transfer/resume after 101.24 seconds. Observation:
`/tmp/autoship-1703-four-parent-observation-5.json`. Host validation passes 582
Python tests and 34 focused managed Rust tests (24 routine guest opt-ins ignored).
The original ten-case unit-policy module and its native count assertion remain
unchanged; the three additional loader identity tests live in a separate module.
No parent-state transfer, input-closure/CI authority or production rollout is
enabled by these observations.

Actual mountinfo is now observed before the original unit's first instruction,
bound to the exec-entry digest, rechecked by Root before enrollment and compared
again while the original cohort is frozen. Parsing preserves escaped and
non-UTF8 pathname bytes and rejects malformed/duplicate mount identities under
the existing 16 MiB/100000-entry bounds. Namespace filesystem labels remain
distinct from ordinary path roots. The observation records exact per-mount
read/write, propagation, filesystem and backing-root fields; it does not yet
classify the complete writable layout or certify private-W writer isolation.
Transfer/resume, input closure, full CI and shipping authority remain disabled.

Replay `5d19f372` verifies 16 retained mount layouts and four nested root children
making progress; original cleanup remains verified and the full test is red after
101.96 seconds at the held transfer path. The pinned guest's chroot table omits
the base mount. Each contained view exposes 20 explicit runtime/source/target,
private temp/home/Cargo/shm, proc/device and transport binds in this fixture.
Observation: `/tmp/autoship-1703-four-parent-observation-6.json`.

Root enrollment now requires exactly the original view's planned mount paths and
access modes, rejects stacked/unknown/shared mounts, and compares actual bound
inodes to the original runtime, source, artifact, working-copy and transport
sources. Proc must be the readonly proc filesystem; writable device binds must
match the original devices. The source/working rules come from the admitted
view and its bound mount manifest. No tool argument or environment can add one.
This enforces the explicit binds; private base-tree permissions, whole-UID writer
closure and complete kernel-resource disposition still need evidence before any
parent transfer/resume or passing-CI authority.

Replay `13ed3156` encountered guest ENOSPC before compiler work: Cargo could not
create its target directory (153.23 seconds, zero compiler calls). Its three
initial metadata units executed and cleaned up, but nested binding enforcement
was not exercised. Managed read-only inspection found 36 sealed generations;
all contain newer execution-lease lifecycle metadata that the existing generation
reclaimer does not understand. Fifteen leases report closed (three lack current
completion evidence), and 21 remain active. This is not deletion authority:
closed-state grammar, completion/cleanup correlation, owned namespaces, live
source protection and original-R disposition must be validated before reclaim.
Inspection: `/tmp/autoship-1703-own-cache-capacity-inspection-2.json`.

After normal source/cache seeding and before each original Cargo invocation, the
controller now rejects zero worker-available blocks/inodes using statvfs and
preserves ENOSPC/EDQUOT through the existing infrastructure path. Root-reserved
blocks do not count as worker capacity. This check does not prove the required
plan fits, reclaim unknown state, increase any cap or replace lifecycle-aware
retirement and required-plan capacity acceptance.

The unchanged full-image replay at `c3166100` now returns the admitted VM's
environment/resource recovery result with guest `OSError` errno 28 at
`prepare_generation`, before repository worker execution (78.63 seconds). Original VM
resource teardown remains verified. This validates the infrastructure boundary,
not a passing Cargo build or recovered capacity. The fixed diagnostic does not
identify which setup allocation/check raised ENOSPC; the capacity guard itself
is directly covered by the host statvfs tests. Host validation passes 589
Python tests and 34 focused managed Rust tests; the VM adapter suite runs 20
cases with six opt-ins skipped. Observation:
`/tmp/autoship-1703-capacity-guard-native-observation-1.json`. Lifecycle-aware
retirement of validated closed executions and original-R recovery of active or
incomplete states remain required; no image wipe, cap increase, new producer
authority or production rollout occurred.

Generation retirement now shares the execution owner's versioned lease and
completion validators. It accepts newer lifecycle namespaces only after a closed
scope, matching command/claim/completion digest, complete predecessor history,
and owned directory/executor identities are verified. Active, legacy/incomplete,
protected and foreign allocations remain retained. This grants deletion of owned
derived state after cleanup; it grants no artifact reuse or passing-CI authority.

Version-2 retirement intents bind immutable evidence records by digest. Projection
and lifecycle evidence each retain the existing 16 MiB record limit, while the
master stays small. The intent is persisted before deleting any generation data.
Replay checks remaining metadata against that original evidence, including after
the state file or individual completion records were removed. Changed evidence
holds; an absent generation can only complete its original validated intent.
Nine filesystem regressions cover protected/active/foreign state, interrupted
deletion, lost final acknowledgement, damaged evidence, unpublished master,
incomplete/cyclic history and large projection metadata. Privileged reclamation
and real four-parent progress still require native VM evidence.

Native replay `d06fdbad` verifies two version-2 closed-generation retirements
through normal reclamation, retaining all 21 previously active generations and
all three incomplete legacy records. The actual mount-binding checks pass for
16 contained entries (20 binds each), with all four nested root children making
progress and original VM teardown verified. The four deepest children return
SIGBUS; none completes successfully, and no ELF result is sealed. The full test
remains red after 106.84 seconds. Post-run free blocks are below the filesystem's
root reserve, but the signal's cause is not independently established.
Observation: `/tmp/autoship-1703-execution-retirement-native-observation-1.json`.

A bounded read-only census of that generation finds 186627618 logical bytes in
the seeded Cargo registry and 28664677 in the contained executor, across 5332
entries with none skipped. Closed-generation retirement is now functional, but
capacity admission and retirement of interrupted executions using original R
evidence remain unfinished. No active/incomplete allocation, cache image or cap
was cleared or enlarged. Host suites pass 598 Python tests and 34 focused managed
Rust tests; transfer/resume, full CI and production autoship remain disabled.

An interrupted execution can now become explicitly nonreusable only after its
original stopped-scope token matches the bound scope and its original cleanup
receipt is revalidated. The controller writes a dedicated abandonment disposition
before committing version-3 `abandoned` state. It never fabricates a completed
command, producer success or reusable checkpoint. Pending/lost state acknowledgement
blocks reuse and can replay only the same original disposition; an already closed
claim is revalidated without being rewritten as abandoned.

Both protected Cargo and repository callers use the same failure path. Nonzero
supervisor results, malformed payloads, input-journal failures and checkpoint
failures after verified R trigger abandonment. An earlier transport failure can
use only that same original scope's verified cleanup; missing/failed verification
leaves the claim active. Retirement accepts the abandonment only after matching
the state preimage, disposition digest, scope/claim/command, predecessor history
and owned directories. Creating/partial executor state remains nonreusable and
can be deleted only under that cleanup authority. Old active/incomplete records
without such evidence remain retained. Zero-capacity admission now also runs
after normal reclamation and before allocating/seeding a fresh generation.

Native replay `32e59bdf` verifies the prospective failure disposition in the
real retained image: version-3 abandoned state and its matching Root-owned
disposition validate; completion remains null and all authority flags remain
false. Normal reclamation retired two more previously closed generations; all
22 historical active generations remained. All four deepest tasks returned zero
and four sealed ELF outputs were verified. The full original four-parent test
still fails with InputCensusUnknown after 104.02 seconds because parent result
application/resume remains fenced; this is not a passing CI or autoship result.
The host suites pass 611 Python tests and 34 focused managed Rust tests.

The original unit exec-stop path now checks the private base tree outside the
shared capture-root list and independently validated mount points. Remaining
entries must be controller-owned, without writable group/other permissions,
ACL/xattr metadata, special entries or unaccounted regular-file hard links.
Descriptor-relative scans refuse symlink mutable roots, compare two bounded
inventories and preserve original atime. The same base fingerprint is checked
again under the original cgroup freeze before capture. This closes a private
base DAC prerequisite only; whole-UID writer/alias/kernel-resource closure and
checked transfer/resume still remain required. All CI authority flags stay false.

Native replay `ae25e429` now retains the original entry/layout/binding records
beside each controller-owned view before the first instruction grant. Managed
read-only inspection validates all 23 unit boundaries, including the original
compiler/probe units whose scope receipts otherwise disappear with the guest
controller tree. Each private base has 23 checked entries; all 16 frozen-parent
base hashes join to the same original boundary and scope. The admitted mount
counts are 15 for initial views and 20 for detached working views. All four
deepest children returned zero; two sealed ELF outputs were independently
verified. The full original test remains red with InputCensusUnknown after
104.57 seconds; original VM subtree cleanup is positively verified. This is
private-base prerequisite evidence, not complete writer/alias/resource closure,
result application/resume or passing-CI authority. The host suites pass 617
Python tests and 34 focused managed Rust tests.

Original enrollment and frozen capture now census all visible procfs tasks for
the unit's assigned UID, not only cgroup members or process leaders. Two bounded
passes compare exact PID/TID/start-time identities, including each thread's
credential fields; a matching task outside the expected cohort holds. Expected
cohorts retain the existing 512-task bound, and global enumeration has a
100000-entry ceiling. The guest controller must share the PID namespace of
procfs PID 1, and the original unit must share that namespace before enrollment.
Entry observations are retained before grant; pause observations bind the full
cohort to the original freeze. Subsequent capture checks require the same
private cgroup to remain frozen with the same process membership; thaw clears
that original barrier. Seven actual-process tests cover escaped same-UID tasks,
wrong births, foreign ownership, vanished tasks, cancellation and bounds. A
standalone census never supplies freeze, cleanup, alias/input or CI authority.
Remaining structural allocation/alias/kernel-resource gates and result
application/resume still stay fenced.

Native replay `003a22c8` independently validates 20 original/nested entry UID
censuses and all 16 frozen-parent censuses against their original task births,
unit UIDs and controller PID namespace. Original VM cleanup is positively
verified; all four nested root children progress. The full test still fails
with InputCensusUnknown after 105.70 seconds because effect application/resume
remains fenced. A further controlled credential-resampling regression exposes
an unrelated bootstrap UID transition causing a false hold: identity changes
now hold only if they concern an expected task or enter/leave the target UID.
Both directions remain tested; this does not ignore any target-UID identity
change. The additional two policy tests do not claim actual host UID changes.

Final replay `4aadc3e0` independently validates 23 entry UID censuses and all
16 frozen-parent censuses after the foreign-transition fix and entry deadline
check. Paused cohorts contain 1, 2 or 5 tasks, matched exactly to the original
PID/TID/start-time observations and unit UID. All four deepest children return
zero; one sealed ELF output is independently verified before sibling
cancellation. The full unchanged test remains red with InputCensusUnknown after
108.35 seconds, and original VM subtree cleanup is positively verified. All
626 host Python tests and 34 focused managed Rust tests pass. These are census
and pause-barrier observations, not complete allocation/alias/resource closure,
parent transfer/resume, passing CI or production autoship.
