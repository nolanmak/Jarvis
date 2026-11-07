# Nested producers and publication barriers

Status: mechanism draft for C/#1807; not an implementation handoff or a passing-CI
contract. Build steps 1–3 can establish feasibility during review and grant no
publication/reuse authority. This extends decisions 4, 5, 7, 9 and 10 in
[ci-cargo-planner.md](ci-cargo-planner.md). The draft must pass independent fresh
and seam review before mechanism handoff. The existing original regression remains
required, unchanged: `live_shared_ci_in_command_generated_publication`.

Review state: all 84 mapped seams of the preceding draft were reviewed. Findings
were consolidated into the decisions and explicit gates below. A subsequent full
fresh-reader review found the three parked implementation choices listed in Open
threads and no additional substantiated wrong-build contradiction. The revised
contract still needs its complete seam review before handoff; these reviews do
not establish implementation, input coverage or production readiness.

## Outcome and boundaries

A wrapper must actually execute, including its original argv, environment,
stdin, inherited verified jobserver pipes, file writes and compiler invocation.
The parent Cargo process stays UID 1001 and cannot execute candidate code under
that credential. Ordinary repository workers remain UID 1000. Each opaque unit
gets its own existing unit ordinal and UID/GID `2000 + ordinal`, private root,
mount/IPC/network namespaces, home, temporary storage and writable outputs.

The entire #1807 D1–D8 and #1703 scope remains required. This mechanism does not
complete runtime/input coverage, trusted third-party seeds, the final shared C
proof/API/audit, all-caller deferral, baseline or reservation/dispatch enforcement,
legacy-writer cutover, normal rollout, Discord or live/soak acceptance. Every
experimental capture retains false complete-input/full-CI/shipping flags.

The same registered target, cache image, resource ceilings, inherited leases,
original command deadline and compute/accounting grant apply. No source-name,
counter-variable or fixture detection selects behavior. No original fixture,
argv, stdin, counters or acceptance conditions are relaxed. Unknown work holds.

## Terms

* **Unit**: one original planned compiler or opaque producer with one unique
  credential and a controller-owned original supervision scope.
* **Contained call**: an authenticated execution request from an opaque unit, or
  an ordinary worker's Cargo request. Its key is the original command plus the
  controller's request ordinal and original scope. A worker-origin input capture
  binds that exact worker-to-Cargo call. Each child owns original supervision;
  the logical parent is recorded separately from physical process ancestry.
* **Cargo scope**: the original supervision scope for one admitted Cargo call.
  Before dispatch, every compiler/application request records this exact Cargo
  scope as parent. Nested containment is opaque unit → contained Cargo scope →
  planned unit, never a global completed-unit slice. Planned requests of that
  Cargo scope are not second contained calls from the waiting opaque parent.
* **Input capture**: immutable bytes copied from a paused parent's private state
  for one contained call. It records an active producer and is not a successful
  output publication, trusted seed, reusable artifact or cleanup receipt.
* **Publication**: outputs sealed only after their original producer and all
  contained calls have finished with verified original cleanup. Failed producers
  cannot publish reusable outputs.
* **Lineage closure**: original scope/cleanup evidence for every producer and
  contained call referenced by an input capture. A later receipt resolves an
  earlier pending relationship; it does not pretend the producer had finished
  when the input capture was made.

* **Provisional execution result**: the original status and committed effects
  returned to a caller within the same still-active original command. It may
  depend on pending ancestor/worker lineage. It is neither reusable publication
  nor a source-correct CI verdict, and another command cannot adopt it.

Containment and dataflow are separate graphs. Captured input E1 precedes child
output E2, which can precede parent output E3. The parent's later cleanup receipt
closes E1's lineage; that receipt is not a data edge E3→E1.

## Decisions and reasons

### O1. Keep writable application state outside the planner

The existing controller owns dispatch, ordinals and publication indexes. It
recognizes wrapper execution from the actual captured request and the controller-
validated Cargo graph/context, not a filename convention. Unknown compiler flags,
ambiguous graph identities or unmodeled wrapper chains retain a typed hold.

An opaque unit sees the immutable original Git source, its admitted dependency
artifacts and the complete permitted generated-input namespaces. Own output and
scratch roots start empty. A declared generated namespace that can be both read
and written is copied into private writable state from its authenticated initial
input; the initial input digest excludes the unit's later writes. Track that
initial read/write state separately from final publication.

The wrapper cannot reach planner metadata, raw cache devices, root controller
files or another unit's writable roots. Its outputs become planner-visible only
through the existing controller publication/materialization path. The broker
copies a completed child's provisional effects into the parent's private outputs before
returning that contained call's original result. Parent alterations thereafter
belong to the parent's eventual opaque output, not to the child's sealed output.

### O2. Give an opaque unit a scoped nested-execution transport

Extend the existing sealed 160-byte V3 configuration with a distinct unit-role
magic. For this role only, its uid field names the assigned client UID in the
existing 2000–6095 range. The expected server UID remains root; worker/planner
magics retain their current semantics. Validate real/effective/saved UID/GID,
empty supplementary groups, sealed descriptor owner/size and root server PID.

Bind the controller's per-unit Unix socket into that unit's private `/run` and
put its virtual path in the configuration. The socket remains root-owned,
accessible only to that unit's GID. Other root controller paths remain hidden.
Require message credentials as well as peer credentials; bind each sender to the
original unit's credential, namespaces and controller-owned containment scope.
Keep original FD validation, jobserver normalization and transport stripping.

Every builder request goes through this transport. Nested Cargo receives the
existing protected planner boundary; it cannot use an ordinary writable target
as proof. Helpers remain within an explicitly admitted opaque scope and its
complete permitted input closure. Unknown executables or an execution route
that cannot preserve mediation produce a hold, never an unguarded fallback.

Mediation must cover the first instruction of untrusted code, including early
loader constructors and static executables. A later preload constructor alone
cannot establish this boundary. The first exec is a distinct controller-granted
event in the original supervised bootstrap; inherited kernel restrictions must
already prevent subsequent raw execs bypassing the broker. Preserve the existing
original exec acknowledgement and test raw exec from both an early constructor
and the program body before handoff.
Select the existing `ExecMediation`/native notification mechanism for this
boundary: the trusted single-threaded bootstrap installs an exec listener after
namespace setup, then installs the unit restriction that prohibits replacement
listeners. It transfers the listener over the root-local sealed-bootstrap FD
with kernel sender credentials to its original supervisor. Only that original
live bootstrap PID may receive CONTINUE while initial authorization is open;
failed PATH candidates may continue there. The original PTRACE_EXEC stop prevents
any untrusted instruction before the supervisor closes initial permission and
completes its existing acknowledgement/detach. Every later raw exec is denied.
This is not authorization of untrusted pointer arguments. Unit, Cargo and worker
entries use the same original-supervisor lifecycle; installation/identity failure
keeps the gate closed and requires original cleanup. Host and pinned-guest
constructor/static tests exist; actual nested helper dispatch and the full
runtime-policy disposition gate below remain unfinished prerequisites.

At `9f6b9a65`, the unchanged native V3 replay reached the private unit relay
in 98.36 seconds. The original build-script compiler (ordinal 8, UID 2008)
submitted request ordinal 9. Its retained root record validates peer/message
credentials, original namespaces/root and the sealed payload. Its exec record
has one acknowledged initial exec and no later raw-exec denial. The earlier
91.39-second replay exposed a missing transport argument at private-view entry;
the second run includes that correction. Both runs remain red overall. The
endpoint deliberately holds before child execution: `dispatch_tested:false`
and `shipping_authority:false`. This proves a real connection, not input
closure, child dispatch, completed CI or production recovery.

At `1dfe92e3`, a further unchanged replay completed in 98.31 seconds and its
retained admission record includes the actual nested request's staged stdin:
zero bytes and SHA-256 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.
The host tests separately exercise nonempty binary streams, sealed bytes and
cancellation from transport loss, original-owner exit, command stop and an
original signal before tool start. All 413 Python tests and 34 focused Rust
tests pass. The guest still holds before parent pause and contained execution;
an stdin record is not an immutable filesystem capture or a child grant.

After namespace setup and before untrusted code, install a kernel policy that
prevents changes to the namespaces/credentials used for attribution: deny
`unshare`, `setns`, namespace-creating clone flags and mount/chroot/pivot APIs.
Refuse unknown ABI routes. `clone3` returns ENOSYS so the pinned libc can use its
existing clone fallback; test that real threads/forks still work. This is a
selected control requiring a real kernel regression, not an existing guarantee.
The same pre-exec policy denies io_uring setup/registration/submission and native
Linux AIO setup/submission. It also covers repository-worker entry, preventing
an ancestor from creating and passing a submission capability before a unit guard
starts. Anonymous submission/device capabilities are not admissible stdio.
Synchronous I/O must have returned before a thread is accepted as stopped. Shared
mappings have no running userspace writer; flush writeback before inventory.
Test an asynchronous submission outstanding at capture explicitly; namespace
restrictions do not substitute for that gate.

#### Runtime-launch disposition observation

The controller checkpoint records kernel-observed launch refusals rather
than guessing from a compiler's errno. C owns classification and the original
scope's record; R's unchanged supervisor owns exec acknowledgement and descendant
cleanup. The constructor-free native helper installs one filter/listener in the
still-single-threaded trusted bootstrap. That filter includes both the existing
unit restrictions and exec notifications. Do not subsequently stack the
standalone unit filter: its stronger ERRNO action would hide the process-launch
notifications. Successful installation is the unit-policy activation recorded
in the original policy receipt. Failure to install or transfer the authenticated
listener prevents original untrusted entry.

Only the original bootstrap's initial exec may receive CONTINUE, before the
original exec stop closes that permission. The listener additionally receives
`clone3`, `vfork`, `clone` with CLONE_VFORK, and separate-process CLONE_VM. The
native receiver classifies the kernel's syscall number and value-valued clone
flags; it never reads `clone_args` or another untrusted pointer. Namespace clone
flags and high flag bits remain EPERM restrictions. The response to every
notified process-creation request is ENOSYS, without CONTINUE, preserving the
existing refusal. With no listener, those requests still fail closed. The
standalone TSYNC guard used outside this original-listener path retains its
existing ERRNO behavior.

A record is appended only after the kernel accepts the response for that live
notification ID. Withdrawn requests are not committed dispositions. Each row
contains the notifying PID, syscall number, clone flags where applicable,
classification, response errno, initial-exec permission and whether the original
exec acknowledgement has happened. Limit the original controller to 4096 rows;
overflow or listener errors retain the existing cleanup/input hold rather than
discarding observations. After the original supervisor returns, the existing
private per-unit `exec-mediation.json` binds the rows to the original
binding/command/ordinal and raw return status. Its exec-denial count counts only
exec rows. This record precedes the typed check, so an unsupported route remains
inspectable even when the application handles ENOSYS and exits zero.

Expected `clone3` fallback alone does not add an unsupported-route hold. A denied
exec, `vfork`, CLONE_VFORK or separate-process shared-mm request does add that
hold, independently of the application's exit status. This distinction supplies
positive evidence about these observed responses, not complete policy coverage.
A stricter stacked ERRNO/KILL filter can mask notifications, and the runtime's
actual supported fallback, unknown ABI, inherited capabilities and input closure
still need their existing gates. Empty notification records are never coverage
proof. Compiler compatibility remains a separate required exit: this observer
does not allow GCC's `vfork`, detach its shared address space, resume the held
parent, materialize outputs or grant C/CI/shipping authority. Before activating
supported-adapter/final-C authority, C must complete the remaining disposition
coverage and bind it through the final C/S consumers; the carrying cost of this
checkpoint is a bounded private observation, with no new durable grant schema.

Keep original child cleanup distinct from the policy verdict. The first real
observer replay confirmed GCC's refused `vfork`, but exposed that throwing the
typed hold before writing `exit-status.json` left the original child loan lent.
After the original supervisor has returned and notification handling has settled
without a listener error, a known unsupported-route hold must retain the actual
raw exit status in a **version 4 held-result** record with
`policy_hold:input_census_unknown`. A normal result remains version 3 with no
policy-hold field. Listener/settlement errors retain their existing cleanup hold;
they cannot create this known-outcome record.

The original relay accepts version 4 only for this exact hold, verifies the
same raw status against its supervised process status and original descendant
cleanup receipt, and performs its existing credit/signal drainage. It may retain
the failed child's diagnostic output, then raises the policy hold before any
effect transfer, publication, terminal child result or parent resume. Successful
unit evidence and both normal/contained effect handlers must require version 3
and reject a policy-hold field even if attached to version 3. An application
exiting zero cannot launder the held result into reusable outputs. Version 4 is
deliberately outside the old version-3 publication contract; cleanup bookkeeping
does not become source or execution authority. C owns this propagation, R owns
the unchanged original cleanup checks. Its testable exit is the unchanged VM
workload retaining raw GCC failure, the observed refusal and returned child
credit while the parent and all effects remain held. The unchanged replay at
`039fd50e` now proves this bounded exit: raw status 1, four kernel-observed vfork
refusals, a version-4 held result, returned child credit, retained parent hold,
private failed-output observation and removal of the original host VM resource
subtree. All 700 Python checks pass with native helpers enabled; all 36 focused
Rust checks pass. The overall worker regression remains red at
InputCensusUnknown because compiler compatibility is still required. See
[checkpoint evidence](ci-runtime-launch-disposition-proof.json).

#### vfork proxy exec feasibility exit (not runtime admission)

This experiment separates **native vfork release** (the kernel stops sharing
an address space and makes the calling thread runnable) from **C parent resume**
(the existing controller permits a frozen producer to run after verified effect
transfer). Neither establishes the other. Linux vfork shares memory and suspends
the calling thread until child exec or exit; see [vfork(2)](https://man7.org/linux/man-pages/man2/vfork.2.html).
The candidate is an actual exec into a separate static status-proxy image at the
existing staging-ready boundary, preserving the original child PID and parent
wait relationship. It does not substitute fork for vfork or run the compiler in
the original writable producer view. The V3 tool-start acknowledgement and
contained tool execution remain separate, later boundaries.

Before choosing that candidate for production, C owns a test-only kernel exit
with R's original descendant supervisor. A native producer issues real x86-64
SYS_vfork; a deliberate raw-ABI shared-memory write distinguishes it from fork
emulation. This Linux-specific observation is not portable POSIX vfork C usage.
Its child attempts native exec of a freestanding static probe. The test listener
may continue this one observed vfork request, then attaches PTRACE_SEIZE with
TRACEEXEC/EXITKILL to its exact child while the child's exec notification is
pending. Only after attachment may the fixture continue the exec syscall. The
tracer thread consumes that child's actual PTRACE_EVENT_EXEC and verifies the
executed inode against its held probe image before detaching. A different static
image is killed at that stop. The kernel stop precedes new-image instructions;
see [ptrace(2)](https://man7.org/linux/man-pages/man2/ptrace.2.html).

The decisive positive observation is the original parent reporting native vfork
return, the actual notified child PID and the shared-memory value while the child
is still stopped at exec and has neither executed probe code nor terminated.
Then the accepted probe verifies inherited FIFO descriptors and kernel CLOEXEC
closure, waits on a private control pipe, and exits with a known status; a
separate case receives a real SIGTERM. The original parent waits on the original
PID and reports the native wait status. Keep original descendant cleanup as the
outer requirement. Private probe pipes are not Cargo jobserver credits and
supply no evidence for the pinned-Cargo saturation/cancellation gate.

Required negative controls are a wrong image stopped before its body, exec
cancellation before CONTINUE, listener loss while exec is pending, and the
unchanged production ExecMediation refusing this vfork. That production control
must remain red for unsupported execution and positively retain its typed hold;
the fixture must not introduce a production environment switch or a permissive
runtime policy. At the fixture boundary, require exact parent/child identities,
single-threaded child, original namespaces/cgroup placement, checked initial
exec acknowledgement, accepted notification responses and original cleanup.
The host and declared VM kernel must both exercise these cases. All fixture
records carry false full-CI/shipping authority.

This is a feasibility exit, not the final C/R architecture or its handoff.
The [seccomp documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html)
does not make continuing untrusted pointer arguments safe. Before runtime use,
C/R must define and independently audit the immutable proxy image/configuration,
exact live admission and notification binding, protection from all original
shared-mm writers, signal and cancellation intervals, pending/failed exec and
PATH-search behavior, pre-exec error reporting and FD cleanup, shared-memory-safe
pre-exec relay code, and original resource ownership without a new grant. A
correct native proxy must preserve V3 streams/signals/status and existing real
spawn tests; this small static probe does not implement that proxy. It also does
not implement captured effect transfer, C parent resume, initial-input closure,
projection, owner productive dispatch or final C/S consumer proofs. C owns these
remaining contracts, R owns process/resource/supervision joins, and the trigger
is before supported-adapter/final-C authority. The carrying cost of this
experiment is test-only native payloads and bounded observations; no durable
permission or source grant is added. The unchanged original compiler and worker
regressions remain mandatory after the real adapter is implemented. The probe at
`3439cda7` passed all six cases on the declared guest kernel in 95.01 seconds,
with the original host VM resource subtree verified removed. All 706 host
Python checks and 37 focused Rust checks passed, with the six native probe cases
mandatory in the Rust adapter. Production refusal remains a passing negative
control. This settles the bounded native-exec feasibility question; it does not
settle runtime admission, shared-mm writer protection or the real V3 proxy. See
[host and VM evidence](ci-vfork-exec-boundary-proof.json).

#### Cross-process memory writers before proxy admission

A separate static proxy image alone does not protect its writable arguments or
stack from its original producer. As a prerequisite for the real V3 proxy, C's
unit filter must refuse process_vm_writev, process_madvise, and every ptrace
operation except the exact full-width PTRACE_TRACEME value. These restrictions
apply to both the standalone TSYNC guard and the combined original listener;
they are installed before untrusted entry. TRACEME declares the calling task's
own tracing relationship; it does not grant control of another task. This is an
operation exception, not a kernel-enforced bootstrap-phase distinction. The
original supervisor's separate exec acknowledgement/initial-permission state
still owns that phase. The trusted controller runs outside the inherited unit
filter and must retain SEIZE, EXEC observation and cleanup behavior.

The reason is integrity of the future image/configuration and capture barrier:
[process_vm_writev](https://man7.org/linux/man-pages/man2/process_vm_readv.2.html)
can change another address space, and
[ptrace](https://man7.org/linux/man-pages/man2/ptrace.2.html) can change another
task's memory and registers. Remote memory
[advice](https://man7.org/linux/man-pages/man2/process_madvise.2.html) is also an
unsupported mutation/control route in this adapter. Refuse all calls to the two
remote-memory APIs, including self-targeting calls. Preserve process_vm_readv;
a checked read is not a write permission or complete input proof. Reject high
ptrace request bits as well as non-TRACEME low values. No new runtime permission,
parent resume or vfork continuation follows from these denials.

The native regression must use a real related-process victim: first modify eight
bytes successfully and attach/interrupt/detach successfully before installation.
Have that victim verify the actual changed bytes and restore its original data.
After guard installation, require EPERM from the same valid remote-write and
attach operations, read back the unchanged original bytes, and have the victim
verify them before actual reap. Exercise both guard variants. The trusted test
setup creates the victim before installation for those positive controls; this
is not evidence that a filter can cover arbitrary pre-existing untrusted child
processes. Existing/new thread tests must also exercise the write restriction
under TSYNC. Preserve the original bootstrap/exec cases and the separate real
vfork handoff probe. The remote-advice refusal test is not a positive capability
or compatibility proof for that API. Require the standalone and listener cases
on the declared VM kernel as well as the host; original resource teardown and
false full-CI/shipping flags remain mandatory.

This closes these specific syscall writer routes, not all shared-mm writer
coverage. Producer proc mounts must still be verified read-only; inherited
memory/proc descriptors, writable aliases, shared mappings, async writers,
unknown ABI/policy routes and mutable pre-exec pointers retain their existing
C/R gates. No errno-only observation is full policy coverage. C owns the guard
and testable denial exit; R owns unfiltered trusted tracing and original
supervision/resource joins. Before final-C or proxy runtime admission, C/R must
complete those remaining writer/input/configuration proofs. The carrying cost
is a stricter measured helper whose identity must travel through the existing
activation/runtime binding, with no durable permission-schema change. The real
V3 proxy and unchanged pinned compiler/worker regressions remain unfinished. At `bfffecd9`,
all nine policy cases passed on the declared guest kernel in 90.19 seconds,
including both successful pre-policy writes/attaches and their post-policy
refusals with unchanged victim bytes and actual reap. The same stricter helper
then passed all six native vfork/exec-boundary cases in 92.96 seconds, retaining
original bootstrap and unfiltered controller tracing. Both original host VM
resource subtrees were verified removed. All 708 host Python checks and 37
focused Rust checks passed. See [bounded writer-route evidence](ci-cross-memory-writer-proof.json);
these observations do not establish the remaining complete writer or input
proofs.

#### Post-ready V3 status/signal component

Implement a separate static `ci_cargo_status_proxy` image for the part of
`delegate_exec` after staging-ready. C owns the component; R retains the original
PID/wait, execution observation, resources and cleanup. Share its response,
signal-forwarding and terminal-status code with the existing V2/V3 relay, so a
second implementation cannot quietly disagree with that protocol. The existing
relay still performs its original signal setup, CLOEXEC closure and wait; this
extraction does not switch callers to native proxy exec. The separate image
must have no ELF interpreter and takes exactly one numeric inherited config FD
with an empty environment.

**Transport continuation is not execution admission.** The 160-byte original V3
root configuration and live admission remain separate from the following
64-byte sealed continuation. This component verifies that its inherited channel
matches the continuation; it cannot establish that the continuation was approved.
Before runtime native exec, the original controller must independently bind the
immutable image, continuation bytes/FDs, actual root peer, live staging operation,
original model/source/owner/resource context and current kernel notification.
Root V3 server-role/credential/group policy remains mandatory in that admission.
Neither a valid capsule, a matching peer supplied in it, nor component exit zero
can replace that proof. Host/guest same-UID socket tests are protocol conformance,
not proof of runtime root-role admission. There is no runtime admission switch
or model-selectable bypass in this component.

Use this fixed little-endian x86-64 continuation layout, with no appended data:

| Offset | Field | Meaning |
| --- | --- | --- |
| 0 | magic, 8 bytes | JARVPXY3 |
| 8 | version, u32 | 1 |
| 12 | connection, i32 | inherited connected control FD, 3..65535, distinct from config |
| 16/20/24 | client_pid/client_uid/client_gid, u32 | exact current real/effective/saved identities; client UID/GID nonzero |
| 28/32/36 | server_pid/server_uid/server_gid, u32 | exact current SO_PEERCRED, PID nonzero and distinct from client |
| 40 | socket cookie, u64 | nonzero exact SO_COOKIE of the inherited client endpoint |
| 48 | deadline, i64 | original absolute monotonic milliseconds, not a refreshed timeout |
| 56 | reserved, u64 | zero |

The config must be an anonymous regular memfd with exactly 64 bytes and all of
WRITE/GROW/SHRINK/SEAL seals. Its owner must be root or the current client; this
is an input-consistency restriction, not provenance/admission authority. Reject
all identity, version, reserved, seal, size and cookie mismatches. Require a
connected AF_UNIX SOCK_SEQPACKET endpoint and a future deadline no more than
900000 ms away. Close only the config FD at setup; keep original stdio and
non-CLOEXEC inherited descriptors intact. The native exec itself closes original
CLOEXEC descriptors. The component sets no-new-privileges/exec denial and disables
dumpability; those actions cannot retroactively protect its pre-instruction
interval from shared-mm writers. The complete writer and immutable-image/config
proofs above remain prerequisites to runtime exec admission.

Staging-ready has already been consumed by the original caller. The component
blocks signals until configuration/channel validation and forwarding handlers
are installed, then forwards ordinary signals as existing JARVSIG3 frames even
if the old caller ignored or blocked them. The launcher must protect the earlier
exec interval too; a signal already discarded before proxy ownership is not
recoverable. The actual delegated tool's requested mask/dispositions remain in
the original argument payload, outside this continuation. Preserve its separate
V3 tool-start acknowledgement: require exact JARVRSP3 stage 2/value 0, then stage
3/exit value 0..255 or stage 5/a representable fatal signal. Retain the existing
fatal-signal reproduction rules. Reordered, malformed, oversized, rights-bearing,
disconnected or expired control is infrastructure exit 125. This exit value by
itself is not a source/policy verdict. The proxy never writes or consumes caller
stdio and does not create another process or resource grant.

The component tests must execute this exact static binary against real sockets
and kernel signals: normal status, pending signals before start, previously
ignored signals after handler ownership, original parent native signal status,
invalid sequencing/control/rights, invalid or unsealed continuation, identity and
cookie mismatch, stream-socket rejection, disconnect and deadline. Verify binary
stdio bytes and inherited FIFO lifetime, including native CLOEXEC closure. Keep
the original Rust spawning/async-Popen/closefrom/stdio/signal tests through the
shared code. Run the component conformance on host and declared guest, retaining
original resource teardown and false full-CI/shipping flags.

The next integration gate remains C/R-owned: actual shared-mm-safe pre-exec
client hook, synchronous errno/PATH behavior before successful exec, immutable
image/config and exact live root-admission factory, all writer control, native
exec observation, V3 signal/cancellation intervals, effect/input/source/resource
proofs and parent resume. Trigger is before any production vfork continuation or
final-C grant; carrying cost is a new measured native asset and fixed input ABI,
with no execution permission or durable grant added here. Root-role integration
and the unchanged compiler/worker/generated-publication regressions remain
mandatory. Do not turn component conformance into an original fixture pass. At `4c437b27`,
all nine component cases passed on the declared guest kernel in 90.67 seconds,
with the exact embedded static image hash matched and the original host VM
resource subtree verified removed. All 717 host Python checks and 38 focused
Rust checks passed, including the nine mandatory component cases and original
relay regressions. These are same-UID wire/descriptor/signal conformance results;
root-role admission and native handoff remain unproved. See
[component evidence](ci-v3-status-component-proof.json).

#### Pre-exec preparation and combined host conformance

C owns `jarvis_ci_prepare_status_continuation`, its close operation and the raw
`jarvis_ci_exec_status_proxy` wrapper. Preparation allocates only two temporary
FDs: a duplicate of the original connected socket and a sealed 64-byte memfd.
It reads actual client/peer credentials and socket cookie, retains the original
absolute deadline and leaves the original descriptor and flags intact. The raw
exec wrapper blocks signals before preparation, passes an empty environment and
only the numeric config FD, and restores the original signal mask and actual
exec errno on refusal. Close both temporary FDs on failure; no fork, fallback
exec or replacement resource grant is permitted. This primitive is data/setup,
not admission, and has no production caller yet. Shared-mm writer/alias safety
and synchronous caller errno/PATH semantics remain integration requirements.

The separate combined fixture uses actual native vfork, the actual static V3
component and a separately supervised native-exec'd tool. Its test-only observer
checks the exact image at kernel exec-stop, validates the continuation against
the original channel, observes the original parent released while the proxy is
still alive, and retains the original parent's native wait status. The tool's
own original supervisor observes exec, reaps it, and supplies actual terminal
status; proxy signals are routed to that tool through its live pidfd. Swapped
channels, wrong images, pre-exec cancellation and the production listener's
refusal must launch no tool. Same-UID host composition does not establish root
role admission, complete writer closure, jobserver accounting, source closure,
parent effect transfer or production activation.

**Original load-bearing constructor conflict:** the ordinary V3 relay
calls `jarvis_ci_deny_exec`, an inherited irreversible ERRNO filter for execve
and execveat. Its refusal precedes USER_NOTIF, so the original controller cannot
approve a status-image exec through that filter. The combined positive fixture
does not load that constructor. An additional combined negative installs the
exact native denial primitive after the fixture's original exec and before
vfork: no child exec notification or exec-stop occurs, no tool starts, and the
original parent reaps exit 93 only after the child dies. Keep this regression;
positive composition cannot stand in for this production path.

C and R own resolution before any production native-handoff admission. The
listener-owned bootstrap path below addresses duplicate-filter ownership; the
remaining native-handoff admission requirements still apply. Trace
all worker, planner and unit bootstraps and their mandatory original listener
installation/exec acknowledgement; preserve early-constructor/static/raw-exec
refusal and actual root-role/live-operation binding. A sealed config, Seccomp
status field, filter count or successful same-UID fixture cannot prove that
policy. Do not remove the constructor denial merely to make the handoff pass.
The carrying cost is a measured native helper and additional conformance cases;
there is no durable grant/schema change. The
unchanged compiler/worker/publication regressions remain required and unproved
for this new composition. This subsection records an intermediate implementation
checkpoint, not a mechanism handoff or a passing original compiler fixture.
Host validation passed 728 Python cases with no skips and 39 focused Rust cases
with 28 opt-ins ignored. The eleven new preparation/composition cases execute
through a mandatory Rust adapter. See [bounded composition evidence](ci-v3-status-composition-proof.json),
including the initial setup errors and signal timeout before the successful
corrected full run. At `c53fb8fa`, all eleven composition/preparation cases then
passed on the declared guest kernel in 102.26 seconds. All four actual guest
native asset hashes matched the embedded build; the original host VM resource
subtree was verified removed without resource exhaustion. The Rust suite with
the new opt-in passed 39 cases, with 29 opt-ins ignored in the bulk run. Guest
clients were UID/GID 1000; this establishes same-UID conformance, including the
production-filter refusal, and does not resolve the constructor conflict or
establish runtime root-role admission.

#### Listener-owned relay bootstrap (C/R policy ownership)

Decision: preserve ordinary V3/V2 constructor exec denial, and use a distinct
canonical configuration only in the trusted root bootstrap after actual native
listener installation. This avoids both an unconditional V3 relaxation and the
irreversible ERRNO filter that masks the original listener. The 160-byte root
configuration keeps all existing fields; only its magic changes: worker
`JARVWRK3` to `JARVWRL3`, planner `JARVCFG3` to `JARVCFL3`, and unit `JARVUNT3`
to `JARVUNL3`. These forms select which layer owns exec refusal; they are not
execution, root-peer, staging, source or resource authorization.

`UnitPolicy.prepare_relay_configuration` is root-bootstrap-only. It requires
actual successful native listener activation in that same process, and runs
after `stage_exec_mediation` transfers the actual kernel listener to the original
supervisor. It creates a new root-owned fully sealed memfd containing the exact
canonical bytes, replacing only the bootstrap's original FD number while
preserving its FD flags and offset. The original server/supervisor copy and its
historical bytes remain unchanged. Worker, Cargo and unit bootstraps perform
this step before dropping credentials and before their initial untrusted exec.
No environment flag, claimed Seccomp state, filter count or model request can
substitute for that bootstrap execution sequence.

The original supervisor receives/authenticates the actual listener and observes
its original child at the kernel exec-stop. Before acknowledgement/release it
independently compares the child's fully sealed root-owned configuration against
the exact canonical form of its own original configuration. Original worker
controller admission independently repeats that comparison while the original
supervisor still holds the stop. Ordinary constructor probes keep their original
configuration and denial behavior. Both old and canonical V3 forms retain exact
role UID/GID/saved-identity/group/owner/seal checks. V2 is unchanged. The new tag
alone does not prove policy installation; the trusted bootstrap, original kernel
listener custody and original exec-stop checks establish this narrower boundary.
Runtime native proxy exec remains denied until its separate live admission and
writer/input/source/resource requirements are implemented.

The private test-only session marker may select a fixed root-controlled
`relay-exec-policy` probe under the existing measured planner probe bundle.
Only repository probe mode accepts it; unknown kinds and cross-operation
selection fail before VM launch. It cannot select source-controlled root code.
The probe loads the actual relay with root-owned sealed role configurations for
worker, planner, first/last unit UID and a descendant. It attempts real execve
and execveat, requires original exec acknowledgement/reap, and records actual
controller denial notifications. Listener-loss must refuse both calls with
ENOSYS. A pre-activation canonicalization attempt must refuse before preparing
the real listener-backed configuration. The returned report remains data with
all complete-input/full-CI/shipping flags false; a diagnostic report with zero
notifications may be retained to explain the RED baseline, but cannot satisfy
the ownership regression. The Rust regression requires two actual controller
denials for every connected case and the expected listener-loss errno.

At baseline `eaeabe91`, the root-controlled guest regression failed after 92.59
seconds: actual worker raw exec calls returned EPERM, original exec/reap
succeeded, but the controller observed zero denial notifications. This is a
real constructor/listener composition failure, independent of the earlier
same-UID vfork fixture. At `19a8ffc3`, all six root-controlled cases passed in 91.64 seconds:
each connected role/descendant produced two actual controller refusals, while
listener loss returned ENOSYS for both calls. The exact embedded native assets
were verified, and both original RED/GREEN VM resource subtrees were removed
without exhaustion. All 733 host Python checks and 39 Rust checks passed (30
opt-ins ignored in the bulk Rust run). This proves the narrower policy ownership
boundary, not runtime proxy admission. The unchanged original worker/Cargo pause
fixture still failed in 115.84 seconds with the same InputCensusUnknown evidence
fingerprint: actual GCC/collect2 hit four refused vfork calls and exited 1. Its
original child was reaped, version-4 held status retained, jobserver loan returned
and host VM resource subtree removed. Do not call that original fixture passing.
See [listener ownership evidence](ci-relay-exec-policy-proof.json).
C and R own those testable exits before runtime native-handoff activation.
Carrying cost is three fixed config tags and exact canonical-byte observation;
there is no durable grant schema or historical owner-preparation rewrite.

#### Proc-memory writer boundary (C/R validation in progress)

The unit view already mounts procfs with `ro,nosuid,nodev,noexec,hidepid=2`.
Use one `mount_unit_proc` helper for that exact production mount and for the
fixed root-controlled conformance probe. Do not infer from the process-vm-write
or ptrace syscall refusals that proc memory writes are covered: they are a
separate filesystem route. Before native proxy exec, C/R must establish that
all relevant untrusted writers have the required proc view and cannot reach a
writable alternate proc mount or already-open writable memory descriptor.

Extend the existing private relay-policy probe with a writable-proc control
and a read-only-proc guard case, both under the actual inherited native policy,
actual V3 unit credentials/configuration and original exec supervisor. Each
client creates its own private mount namespace before native policy activation;
the guard calls the same production `mount_unit_proc`. The control alone creates
a private writable proc mount. After native exec/acknowledgement and dropping
to the assigned UID, the client forks a cooperative same-UID victim with a real
buffer. The parent must read the victim's original bytes through a read-only
memory FD, so DAC/Yama refusal cannot masquerade as a successful filesystem
guard. It then tries direct `/proc/PID/mem`, self-root and victim-root links,
and reopening the read-only memory FD through `/proc/self/fd` for writing.

The writable control must actually write through every alias and observe each
new value in the victim's memory. The read-only case must get EROFS from every
writable open; EACCES/EPERM is not the expected positive control. Attempting to
change the read-only FD's access mode must not grant write access, and pwrite
on that FD must return EBADF. The live victim reports its own final bytes,
which must be changed in the writable control and original in the guard case,
and is then reaped under the original supervisor. Observe the actual mount's
read-only flag rather than deriving it from the requested mode. Report each
alias separately; missing aliases, boolean errnos, changed guarded bytes,
missing read-control/reap or added authority flags reject the report.

This is a bounded filesystem-writer proof under the unit proc view, not a
complete writer closure or runtime vfork admission. Worker/planner views,
foreign writable memory FDs, descriptor ingress/lifetime, mappings, cross-scope
aliases, asynchronous writers, all relevant ABI paths and the original live
cohort/source/resource binding remain C/R-owned prerequisites before admitting
those routes. Root configuration tags and this diagnostic report cannot replace
them. The returned report retains false complete-input/full-CI/shipping flags.
The carrying cost is a shared proc mount helper and two fixed private cases;
no runtime permission or durable grant schema is added. At `b16780dd`, all eight expanded root-controlled cases passed on the declared
guest kernel in 96.60 seconds, including both memory cases and the six prior
relay-policy cases. All four writable-control aliases changed actual victim
bytes; all four guarded writable opens returned EROFS while read controls
succeeded and the victim retained its original bytes. Read-only FD pwrite
returned EBADF in both cases. Actual mount flags, original exec acknowledgement
and victim/supervisor reap were checked. Embedded helper/native artifacts
matched, and the original host resource subtree was removed without exhaustion.
All 735 host Python checks and 39 Rust checks passed (30 opt-ins ignored in the
bulk Rust run). See [bounded proc-writer evidence](ci-proc-memory-writer-proof.json).
Do not rerun the unchanged compiler fixture
merely to restate its vfork refusal before a relevant admission change exists.

### O3. Freeze before capturing a live parent's input

First capture the child's original request, exact stdin to EOF and verified
inherited jobserver descriptors through the existing staging boundary. A parent
still feeding that stdin must not be frozen prematurely.
Here original stdin means the exact bounded byte sequence. The existing staging
contract replaces fd 0 with its sealed memfd; it does not preserve the drained
pipe's object identity, seekability or metadata. That pinned normalization is
part of the execution/input contract and must be modeled before CI authority.

Then acquire the parent's controller mutation lock. Every controller write into
the parent's private outputs, including completed-child materialization, uses
this lock. Pause every live process/thread bearing the parent's uniquely assigned
UID, using root-owned pidfds and recorded births. Namespace/credential restriction
from O2 makes that census meaningful. Re-enumerate until the original UID's
entire population is stopped or zombie and no unaccounted member remains. A
bounded census that cannot establish this state holds; a sampled `/proc` tree,
one stopped main thread or a stopped process group alone is insufficient.

The current feasibility implementation tests a controller-owned cgroup v2 scope
as the structural pause boundary instead of repeatedly stopping sampled PIDs.
Enroll the original single-threaded process at its existing PTRACE_EXEC stop,
before its first untrusted instruction. Descendants inherit that scope; its
root-only controls are absent from the private view. No controller/supervisor
enters it, no resource grant is increased, and nested cgroups are disabled.
Wait for the kernel's `frozen 1` event before reading its bounded process/thread
birth census. A cgroup name or a successful freeze write is insufficient.
Retain the original UID, namespace/root and supervisor bindings. Thaw and
positive original cleanup are separate observations. This follows the
[kernel cgroup v2 contract](https://docs.kernel.org/admin-guide/cgroup-v2.html),
but the pinned guest replay must establish support. Signal routing across a
stopped parent and capture/child execution remain explicit gates below; a
successful freeze experiment cannot authorize either.

The unchanged pinned-guest replay at `62749ab8` reached this kernel-completed
freeze in 98.69 seconds. Its retained original scope contains two processes and
five threads, including the compiler's forked descendant; every recorded member
has its original birth identity. The nested request and empty stdin remain
bound to that same parent scope. The previous admission record at `1dfe92e3`
has no pause evidence. An initial freezer replay at `c793be20` stopped earlier:
root membership was checked before the first unified-hierarchy mount. The
kernel omits that hierarchy from `/proc` until it becomes visible; the corrected
entry mounts first and then verifies root membership. Host validation passes
414 Python tests and 34 focused Rust tests. The full native test remains red
at the intentional capture/dispatch hold. No immutable filesystem capture,
signal-at-pause acceptance, child execution or passing-CI proof is claimed.

While paused and locked, flush and copy only the admitted parent's visible
private input/output state needed by the contained call. Verify held inode
identity, full inventories, modes, ownership, link closure and unchanged bytes;
retain existing unsupported-entry and size/entry bounds. Sync the immutable copy
and commit its root-owned input-capture record before granting child execution.
No writer-owned inode becomes a child input. This is staging, not decision 9's
completed-output publication. That distinction is an explicit amendment.

The initial byte-capture implementation opens the original `/proc/<pid>/root`
while its held pidfd and kernel freeze remain valid, then traverses with held
directory descriptors and no-follow child opens. It stages `workspace/target`,
`tmp`, `home/worker`, `cargo`, `var/tmp`, `dev/shm` and controller-declared generated
roots within the existing 64-root, entry, byte and record bounds. It rejects
unknown owners, symlinks, special entries, hard links and writable stdio aliases.
The original before/after inventory must agree, and a readback of the sealed
detached destination must match its source bytes. A preparation record precedes
copying; only verified/synced bytes receive a captured-state record under the
original request/scope. This is neither successful producer publication nor
child writable-set admission. Original metadata is retained in the capture
record; the detached copies do not yet emulate it. Complete writable-alias
closure, metadata/runtime behavior, signal-at-pause cases and the child grant
therefore remain closed gates, with false complete-input/CI/shipping flags.

At `f8316da4`, the unchanged native replay staged 64,832 bytes across 21 entries
from the actual frozen compiler view. Its 288.25-second elapsed time includes
shared storage admission waiting. A subsequent read-only image inspection under
the managed lease independently verified all four payload files, their digests,
root ownership, sealed modes, complete staged membership and the payload digest.
The original request/scope, capture-record hash, immutable pause hash and separate
verified-thaw observation agree. The preceding 96.54-second run at `5139e30b`
exposed mutation of the pause hash preimage when thaw was added; the fix keeps
thaw outside the frozen observation. All 423 Python tests and 34 focused Rust
tests pass. The full native protocol remains red at the held child grant. These
are byte-staging observations; metadata emulation, complete alias closure,
contained execution and final CI authority remain unproved.

The next implementation increment creates a detached working copy from the
verified capture and routes contained tools through the original supervisor,
exec observation and grant. Working bytes preserve mode and mtime with mapped
actor ownership; inode/ctime emulation and complete alias closure remain open.
Terminal child results are withheld and attempted parents remain frozen until
original teardown: effect transfer is not implemented, so this increment must
end in an infrastructure hold. Parent death cancels further child admission.
Any controller exception fences the shared command, cancels its controllers and
retains the original allowance slot through teardown; uncertain cleanup cannot
admit replacement work. The 437 Python tests and 34 focused Rust tests pass.
At `7a5ad263`, the unchanged native replay completed in 101.74 seconds and
started four successive contained units (ordinals 9–12). Read-only inspection
under the managed lease verified each original exec observation and grant,
zero denied execs/notification errors, and original cleanup markers. The leaf
returned zero. Its parent deliberately withheld the result and stayed frozen
because effects were not materialized; command fencing cancelled the ancestors.
The outer `planner_request_cleanup` hold therefore does not show a failed leaf
cleanup. This does not establish copyback, active-child signal routing,
jobserver loan correctness, complete input closure, full CI or shipping authority.

An opaque parent stays stopped during its contained call. Child stdout/stderr
use their original admitted descriptions; fd 0 uses staged bytes as above. The
controller and external Cargo caller remain runnable. Worker-origin capture uses
only the short pause windows specified in O3b, not this opaque-parent lifetime.
Interactions requiring a concurrently running parent or unmodeled writable IPC
do not have an admitted frozen-input model and hold. This restriction must be
tested against the actual required plan before activation, not assumed harmless.

Existing original supervision remains responsible for cleanup. Pause evidence
contains command/source binding, parent unit/scope, pidfd/birth census, namespace
identities and capture digest. It never contains `all-descendants-reaped`.

Only one contained request may be outstanding for a given opaque parent. A
second request from any of its threads/processes marks the parent incomplete,
revokes the active child and causes checked subtree cleanup and a typed hold.
Never grant the second request or resume the parent while a child is active.
Different parents may use the existing permits independently. This explicit
supported-model restriction requires actual-plan evidence; the multithread case
must observe safe refusal/cleanup, not silently serialized execution.

### O3a. Copy back the admitted mutation set

At child admission the controller records normalized writable set W from the
parent allocations and the validated child action. Child filenames/environment
cannot expand W. Compiler outputs, fresh own OUT_DIR and scratch allocations
are distinct from initially populated read/write generated namespaces. Inputs
remain read-only; unsupported input/output overlap holds.

The initial writable inventory and final inventory after original child cleanup
define additions, replacements, mode/timestamp changes and explicit deletions.
Record each child's initial private inode-to-parent-inode mapping. A final
child inode equal to its initial inode denotes in-place mutation; copy changed
bytes/size/mode/times through a held, identity-validated parent inode, preserving
parent open descriptors and mappings. Match final inodes against the entire
initial inode-to-path graph, not just the original path at that destination.
A moved initial child inode keeps its mapped parent inode at the new path;
only a genuinely new child inode receives a new parent inode. Unlink preserves
already-open old inodes just as ordinary unlink does.
A path inventory alone loses mutations to an old inode subsequently unlinked or
replaced. Retain controller-owned descriptors for the initial child writable
inodes and corresponding parent inodes, within the original FD ceiling. After
child cleanup, read each old inode through its descriptor even when unlinked;
apply its in-place changes before namespace replacement/removal. Unknown alias
closure or insufficient descriptor capacity refuses admission. Test write→unlink
and write→replace with retained FDs and mappings; neither lost effects nor a
larger FD grant are acceptable.
The current implementation retains no-follow descriptors to both original and
working entries before child execution, after comparing the capture and clone
identities, bytes and metadata. Descriptors use the existing kernel ceiling,
close on partial admission failures, and do not survive exec into a unit. After
verified child cleanup, `snapshot_old` reads the held working descriptors even
when their original pathname was unlinked or replaced, verifies the unchanged
parent side, seals the old inode payloads and verifies copied bytes by readback.
The result is an observation with `effects_materialized:false`; it does not
mutate the parent, resume it or complete the transfer intent below. Real host
filesystem cases cover write→unlink, rename-over-existing, directory rename,
parent drift, extra hardlinks, descriptor exhaustion, exec inheritance,
cancellation and payload corruption. At `3d5097d2`, the unchanged guest replay
completed in 100.39 seconds, again ending at the deliberate unmaterialized-effect
hold. Read-only inspection under the managed lease independently verified all
seven old inode payload files (64,832 bytes), their hashes, root ownership and
sealed modes. Their exact binding, command, ordinal and child/parent scopes
agree with the original exec observation/grant, leaf exit zero and cleanup.
The original execution resource subtree was positively removed; the waiting
parent remained held. All 446 Python tests and 34 focused Rust tests pass.
This proves retained-byte integration, not final-path transfer, parent resume,
complete alias closure, full CI or shipping authority.

Prepared transfer plans now seal the final allocation inventory alongside the
retained old inode payloads and original sealed before image. Final inventory
uses explicit tombstones for missing allocated roots; the admission capture
continues to require every initial root. The entire private container is checked
against W and its fixed ancestors, so a new sibling cannot expand the allocation.
The object graph matches final inodes across all initial paths, distinguishing
moves from replacement at the same pathname and preserving old unlinked writes.
Protected objects, hardlink/symlink aliases, parent drift, changed before images,
missing graph objects and interrupted preparation refuse completion. Plans use
the existing entry/byte/metadata bounds and remain `state:prepared` with false
materialization/input/CI/shipping flags. The root controller binds the canonical
plan digest to the original command, parent/child scopes, ordinal and child exit
status; failed origins remain non-reusable. This is preparation only. Durable
application/replay, alias closure and checked parent resume remain required.
All 453 Python tests and 34 focused Rust tests pass. At `7ff16743`, the
unchanged guest replay completed in 101.12 seconds at the unmaterialized-effect
hold. Independent read-only image inspection under the managed lease verified
the canonical intent digest, exact original scopes and inode graph against the
original working-copy mapping and final inventory. It verified all before image
payloads (seven files / 24 entries / 64,832 bytes) and all after image payloads
(eight files / 25 entries / 537,776 bytes), including one new 472,944-byte output.
Old inode payloads and original cleanup/resource teardown also verified. The
parent remained held: application, replay and checked resume are unproved.

The host-tested inode-write phase applies changed old file bytes, mode and
mtime through original writable handles acquired before child execution; root-
owned immutable inputs retain read-only handles. Positioned writes preserve
parent descriptor offsets, inode identity and normal shared/private mapping
behavior. Every before/after image, old payload, original namespace identity and
held child inode is validated before the first parent write. Root-private
progress binds the prepared plan digest, controller PID/birth and each original
inode. A completed write whose progress commit was lost is recognized only by
its exact after image; unknown partial bytes or later foreign writes stay fenced.
Only `state:inodes_applied` is recorded, with false materialization/input/CI/
shipping flags. Namespace edges, directory metadata finalization and parent
resume are separate unfinished phases. This primitive is not yet called by
contained execution: exclusive-writer/alias proof must precede parent mutation.
Real host cases cover retained FDs/offsets, shared and private mappings,
write→unlink/replacement, all-images preflight, partial writes, lost progress
commits, metadata-only changes, foreign writes, late child writes, corrupt final
images and forked-controller refusal. All 462 Python tests and 34 focused Rust
tests pass. At `217959d7`, the unchanged guest replay completed in 102.59
seconds at the deliberate result hold. After a managed inspection admission
deferred behind a live production VM, the admitted read-only inspection verified
the original exec/cleanup and prepared plan graph, all seven before/old payloads
(64,832 bytes) and all eight after payloads (537,776 bytes). The original resource
subtree was positively removed, and no inode-application journal exists. This
checks pre-execution writable-handle acquisition; guest application is unproved
and remains uninvoked.

Prepared plans now derive namespace operations from each object's parent
inode and basename, rather than from a changed full pathname alone. Descendants
carried by an ancestor rename retain their existing edges and are not detached
or reinserted. Changed old edges are ordered deepest first; final attachments
put directory parents before their children. Host filesystem cases cover a
renamed populated directory, rename over an empty directory, two-file swaps,
new directory/child ordering and moving a child before removing its old parent.
All 467 Python tests and 34 focused managed Rust tests pass at `08d06630`.
This is planning only: detach/attach execution, filesystem-local staging,
interrupted-move replay and checked resume remain unfinished. Guest namespace
execution and application remain unproved.

Individual namespace steps now execute and replay no-overwrite moves, unlink/
rmdir and attachment of a prepared anonymous file. A root-private journal binds
one step to the plan digest, controller PID/birth, original held inode and both
parent inodes/names. It captures complete bounded parent listings and the held
inode's bytes/metadata before dispatch. Recovery accepts the exact before state
or the expected kernel transition with the original inode; a filename alone is
insufficient. `renameat2(RENAME_NOREPLACE)` and the new-file link refuse occupied
destinations without clobbering them. Parent/file sync precedes the step's
applied record, whose materialization/input/CI/shipping flags remain false.
Fourteen real host cases cover preserved FDs/mappings and descendant inode/
ctime, parent overlap, directory removal, anonymous-file attachment, lost syscall
acknowledgements or progress commits, a destination race, foreign changes,
prepared-intent storage failure, wrong plans and forked controllers. A separate
opt-in guest check runs these same cases on the admitted cache filesystem;
at `9e96d376`, all 14 cases passed in 87.71 seconds as UID1000 on that cache
filesystem, including the injected acknowledgement/storage/race failures. The
original resource subtree was positively removed. This is guest syscall/replay
evidence, not whole-transfer or parent-resume acceptance. All 481 Python tests
and 34 focused Rust tests pass (with 23 opt-in guest checks excluded).
Whole-transfer sequencing, filesystem-local staging
allocation/new-directory creation, final metadata/materialization validation,
alias closure and checked parent resume remain unfinished. The executor still
does not invoke inode or namespace application.

Namespace sequencing now binds the complete step list and held inode/parent
capabilities, including mount identities, to one plan and controller PID/birth.
Before the first step it simulates all source/destination edges and refuses
later collisions, nonempty removals and directory cycles. A durable master
stores the next step, active operation, immutable step-receipt digests and the
whole current participating inode/parent graph. Recovery validates that graph,
then replays only the active operation; an obsolete completed step is never
re-executed against parents changed by later operations. Active recovery projects
the verified before/after transition across all parent aliases and refuses
unrelated changes before further dispatch. Completion rechecks the entire graph
even if only the final master commit was lost. That last boundary initially
failed its injected foreign-write regression and is now checked explicitly.
Eighteen host cases cover each lost syscall/master commit, swaps, directory
rename-over-existing with unchanged descendants, anonymous replacement, child
moves/removal, invalid later operations, unrelated writes and final-commit loss. A second
regression caught late inode-mode refusal after an earlier move; all inode
mode/attribute admission now precedes the first step.
This sequences already-held namespace capabilities only: whole-transfer image/
allocation preflight, owned staging/new-directory creation, old-inode-write
integration, final metadata/materialization validation and alias/pause proof
remain caller-owned and unfinished. The namespace master still records false
materialization/input/CI/shipping flags. All 499 Python tests and 34 focused
managed Rust tests pass. At `d6721c51`, the expanded pinned-guest check passed
all 32 cases (14 original step cases plus 18 sequence cases) in 89.48 seconds
as UID1000 on the admitted cache filesystem. This includes both regressions
above, each lost syscall/master-commit boundary, old-inode anonymous replacement
and preservation of directory descendants. Original resource teardown positively
verified. This establishes namespace sequencing, not whole-transfer allocation/
image validation, staging creation, alias closure or checked parent resume.

Per-transfer object preparation now owns verified anonymous replacement files
and named directory capabilities under the existing parent mutation lock. File
bytes are copied from sealed controller payloads, checked by readback, mapped to
the admitted owner/group/mode/mtime and synced while link count stays zero.
Directory creation records its intent before `mkdir`; preparation records the
actual held inode after sync. A known prepared identity can be reopened only
when the named inode matches. Lost creation acknowledgement without a recorded
or still-held capability stays unknown; it cannot be adopted by filename.
Object/byte reservations include preparing receipts across all keys and retries
under the original 100000-entry/4-GiB ceilings. A holder nonce prevents a fresh
holder replacing another holder's unknown anonymous capability. Four injected
regressions first reproduced duplicate reservation use and anonymous capability
replacement; shared reservations and capability ownership now refuse them.
Fourteen real host cases also cover corruption/FD cleanup, metadata admission,
prepared-commit loss, existing foreign names and forked-holder refusal. These
primitives require the original serialized controller mutation lock and its
allocation/writer/pause proof; they introduce no new runnable grant or resource
pool. Whole-transfer allocation/preflight integration, final metadata/graph
validation, alias closure and parent resume remain unfinished. Guest preparation
at `e43c654d` passed all 46 cases in 90.50 seconds as UID1000 on the admitted
cache filesystem: the unchanged 14 syscall and 18 sequence cases plus these
14 creation/reservation cases. Original resource teardown positively verified.
All 513 Python tests and 34 focused managed Rust tests pass. This proves the
creation/recovery primitives; whole-transfer integration, allocation/image
proof, final materialization and parent resume remain unproved. All
materialization/input/CI/shipping flags stay false.


`RetainedTransfer` now composes the filesystem primitives in one original
controller lifetime: validate sealed/current child images and allocation,
apply old inode effects, allocate held new objects, run the namespace sequence,
remove its private stage, apply final directory metadata, and validate the whole
final graph and all retained old objects. The phase journal skips completed
components after later legitimate namespace/metadata changes. Lost phase commits
are checked against the same holder's exact current graph before persistence;
a new holder cannot adopt an anonymous capability or partial transfer. Directory
metadata recovery projects only the active inode's exact intended transition
across all parent aliases and rejects unrelated changes.

The combined tests exposed stale child metadata in the namespace sequencer's
ancestor views; projection now refreshes every held directory's child list,
including ancestors whose own inode metadata did not change. Another injected
failure reproduced refusal after a completed directory metadata update lost its
sync acknowledgement; exact whole-graph recovery now recognizes it without
rewriting the metadata. Seventeen host cases cover combined write/replace/unlink/
move/create operations, swaps, ancestor-carried descendants, root tombstones,
retained FD/mapping semantics, corruption before mutation, lost phase commits,
foreign writes and lost-holder refusal. The expanded guest test retains all 46
previous cases and adds these 17 cases. At `7b0e6723`, all 63 cases passed
in 92.35 seconds as UID1000 on the admitted cache filesystem. The original
resource subtree was positively removed after cleanup. All 530 Python tests
and 34 focused managed Rust tests pass; 23 opt-in guest tests remain ignored
in routine Rust runs. Receipt: `/tmp/augmentagent-ci-whole-transfer-20261104064537/receipt.json`.

This is still a filesystem-mechanics component. The root executor does not call
it or resume its paused parent. Original alias/exclusive-writer/W/pause/cleanup
proof, active-leaf signal routing and jobserver loan remain required before that
integration. Staging uses an unchanged mutable directory inside W; unsupported
staging/mount topologies hold before old inode writes. All final/old file bytes
share the existing 4-GiB ceiling; all original/new/stage nodes share the existing
100000-entry ceiling. No external temporary tree or new grant is introduced.
Input closure, CI, materialization and shipping flags remain false. This draft
and the final C proof still require the full independent handoff audit.

A rename is an identity-preserving namespace move in this graph, including
rename-over-existing and directory descendants. Persist its original/final path
sets, source/destination identities and staged-move progress in the transfer
intent. Replay validates the whole admitted graph transition or holds before
parent mutation; delete/add of fresh unrelated inodes is not equivalent. Test a
parent write through a retained source FD/mapping after rename-over-existing,
directory rename and each interrupted move. Unchanged entries
keep their original inode/metadata. External hard links and unaccounted writable
aliases refuse admission. Record inherited writable stdio inode capabilities;
a pathname inventory alone does not establish exclusive writers. If the required
in-place semantics or alias/writer closure cannot be proved, hold before mutating
parent state. Test retained descriptors and shared/private mappings explicitly.
Paths outside W cannot change; deleting a directory with a surviving entry outside
W refuses materialization.
Before any child instruction, refuse writable regular-file stdio overlapping W
or a worker-projection destination. Recording an alias does not authorize a
second direct mutation path. Other admitted regular output descriptions retain
their original identity/offset and are recorded as direct execution effects
outside copyback. Required-plan tests must prove this restriction fits; otherwise
C must implement one authoritative backing/offset model before handoff.

Keep the parent stopped and its mutation lock held. Persist an intent containing
W, both inventory digests, original affected identities, mutation kinds and
sealed before/after images or tombstones. Stage/sync replacements, validate old
identities, apply identity-preserving writes or actual replacement/removal as
recorded, and sync files/directories before committing completion. An interrupted
in-place write is recognized only by an authenticated before/after image or a
previously durable progress range; an unrecognized partial state stays fenced.
Replay cannot guess from final filenames or overwrite changed foreign bytes.
This is recoverable filesystem mutation, not a cross-store atomic transaction.
The parent cannot resume or receive a terminal child result before completion.
Interrupted transfer stays fenced under original R ownership; replay validates
every already-applied delta or holds before any parent resumes.

Preserve original failure effects within W too, including counter/log writes
before a failing compiler call. Such materialization is an execution effect for
the waiting caller, never reusable output publication. Failed-origin effects are
marked non-reusable and cannot seed a later green check. Only successful closed
producer outputs enter the reusable artifact index. Project admitted workflow-
visible effects to the declared worker namespace before returning Cargo's terminal
result, preserving unchanged original files and recording successful/failed origin.
No failed result becomes success via copyback.
Carry origin dispositions transitively through child copyback, parent sealing
and worker projection, including failed-origin ranges and namespace effects.
A parent's exit zero does not erase a child's failed origin. Affected outputs
stay outside the reusable index unless a later successful, completely attested
production establishes a new output version; matching bytes or an ordinary
overwrite is not that proof. Counter/log effects remain execution history and
do not acquire reusable-artifact authority.

### O3b. Authenticate and quiesce the worker projection

At original command admission the existing pinned CI contract/output allocation
provides the worker projection map for declared generated namespaces, with exact
workspace-relative source/destination roots and file/directory types. It excludes
tracked Git source, .git, planner metadata and raw cache/controller paths. A unit
path or environment variable cannot add a destination. Record the map, original
Binding, command/worker scope, source publication/effect, initial destination
inventory/identities and worker-projection epoch in the same transfer intent.
Cargo artifact exports are a separate controller-owned part of this map. Derive
their exact file/alias slots from the admitted Cargo graph, target/profile/action
and validated compiler outputs, not the Node generated-output declaration.
Project only sealed worker-consumable artifacts needed by the admitted caller,
including its post-Cargo executable alias. Never expose planner fingerprints,
incremental state, locks, private metadata or the whole protected target. Test
the unchanged worker's post-Cargo binary execution and stale/missing artifact slots.
The generated map is known at original command admission. Artifact slots are
added by the controller after admitting that exact Cargo call's immutable graph
and invocation, before build dispatch. Extend/sample their destination baseline
under the same bounded worker pause/lock and commit the map with that call's
capture; an unplanned path cannot become an export later merely by existing.

Before worker-visible projection, release parent mutation locks while keeping its
execution gate closed. Acquire the command's one worker-projection lock. Pause
the original UID1000 worker population under its original command scope, using
O2/O3's guard and census rules. Also pause or positively drain every registered
scope holding an inherited writable descriptor alias to affected destinations.
Unknown writers/aliases or inability to establish quiescence hold. Never wait for
a worker lock while holding a parent lock. The VM admits one ordinary repository
worker command; other worker ownership is a refusal, not a reason to stop an
unrelated process.

Revalidate destination ownership, ancestors, inode/link identities and expected
epoch/inventory after that pause. A newer worker change is a conflict: hold
without overwriting it. Apply the same inode-preserving delta/intent/replay rules
as O3a. Commit the destination epoch and origin disposition before resuming any
writer or returning Cargo's terminal result. On interruption the original R
fence owns recovery; no caller sees an acknowledged transfer before it commits.
Failed-origin projection remains execution history, never successful generation
or reusable-artifact authority. Test changed destinations and crash mid-projection.

Staging initial generated inputs changed by the active ordinary worker uses the
same paused-input-capture protocol before Cargo dispatch, with parent kind
`worker` and the original command scope instead of a unit ordinal. It does not
borrow a previous completed step's producer receipt. Later closure uses that
original worker's actual exit/cleanup, not an invented unit cleanup.
The worker pause ends after the capture and its baseline commit, before Cargo
runs. It remains runnable to drain streams and await Cargo. Pause it again only
for bounded projection critical sections. The opaque-parent rule of remaining
stopped throughout child execution does not apply to this worker.

The projection epoch belongs to the original command/worker scope, not to the
successful-publication index. Initialize it to zero at command admission. Under
the projection lock and worker pause, each worker-to-Cargo capture commits the
actual destination inventory and its expected epoch as that call's baseline.
Each committed capture/projection transaction advances the epoch once, using its
durable transaction identity so replay cannot advance it twice. A prior committed
projection is the next transaction's baseline; never reuse the command's original
inventory or resample away an intervening worker edit. Persist expected/next
epochs, original identities and origin in the intent; validate both after pause.
Replay accepts its own committed next epoch, while an unrelated later epoch or
changed affected destination holds without overwrite.

The original repository command now retains a private `repository-command-result.json`
in its existing command-owned jobserver-credit directory after its original supervisor
cleanup check and before planner holds suppress the public result. It records the
unchanged supervisor return code and each already-captured stdout/stderr stream's full
byte count and SHA-256, with at most 32 KiB at each edge encoded as hex. Binary bytes
are not decoded into controller fields; omitted middle bytes are explicit. The
existing combined 8 MiB capture ceiling is unchanged, and the record is bounded to
512 KiB. No pipe, descriptor, stream ordering or caller output is replaced.

The first diagnostic is created privately and never overwritten. An identical retry
reuses it; changed, linked, incomplete or foreign data refuses replacement. It is
inspection data only, retaining false input-closure/full-CI/shipping flags. It grants
no cleanup receipt, effect materialization, parent resume, artifact reuse or kernel
policy disposition. Inspect it only under the existing managed cache lease. This
closes the missing failure-output observation before the next original worker replay;
the deliberate contained-effect-transfer hold and every O1–O6/final-C gate remain.

A failed contained child can leave its error bytes in a pipe whose reader is the
frozen parent, so the outer repository record alone cannot recover that failure.
After checking that child's original cleanup marker/raw status and settling its
existing credit/signal cleanup gates, the unit relay rechecks its actual parent's frozen cohort and records a non-consuming snapshot
of available stdout/stderr pipe bytes in the same private command directory.
It saves only FIFO identities before the launcher closes forwarded descriptors.
After cleanup it finds an exact matching reader in the actual frozen task cohort,
checks the original task birth and FIFO identity, then uses nonblocking `tee` into
a bounded private pipe. It retains no extra writer descriptor during execution
and closes every newly opened reader/scratch descriptor before returning.
It never consumes the original pipe, changes its flags, reads regular files/sockets,
thaws a parent or acknowledges a child result. The sample is at most 32 KiB (the
kernel may return less); it is always marked `complete_stream:false`, including
empty or unavailable observations. Sample digests cover sampled bytes only.
The original command/global request ordinal keys `contained-output-<ordinal>.json`;
it binds the original Binding, parent/child scope IDs and verified raw return code.
It uses the repository diagnostic's same first-record/identical-replay rule; a
collision or changed record is refused rather than replacing an earlier child.
The record keeps all input-closure/full-CI/shipping flags false and is diagnostic
data, never an input census, nonrelease, policy classification or cleanup grant.
If this observation refuses, its original private scope retains only a bounded
stage, exception category, errno and typed freshness reason, without raw exception
text; the refusal is propagated rather than authorizing resume.
This observation is not final stdio-fidelity evidence for parent-loss or external-alias
races; those O4 cases still gate activation. Actual child/parent authority and every
deliberate copy-back gate remain unchanged.

### O4. Preserve bounded execution and cancellation

Use one controller-owned ordinal/request budget across all relays, preserving
the existing 4096 request/UID bounds. The existing four execution permits apply
to runnable units. A paused parent lends its permit to its contained child; it
cannot resume until that child releases it. Waiting parents do not create extra
runnable work. No operator CPU/RAM/PID/FD ceiling or command deadline increases.
The admitted nested jobserver model lends the parent's one implicit job slot
alongside that controller permit. Persist one command/parent/child credit-loan
identity; only the active leaf may use it, and parent resume waits for its checked
return. This is a logical handoff, not a pipe write: never return or fabricate an
implicit token. Any additional real tokens stay with their actual holder and
must be returned exactly once by that holder. Preserve the original pipe bytes,
flags and normalization. This follows the [GNU jobserver implicit-slot model](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html);
it does not establish that every nested builder obeys it. Before pause, refuse
requests whose credit chain/model cannot be established. The actual pinned-Cargo
four-parent test must exhaust both controller permits and pipe tokens, run nested
work with the borrowed implicit slots, and verify cancellation/reacquisition
without token injection. Failure blocks handoff and requires a revised model.

The current controller now records `credits.json` once inside the original
admitted generation's managed target, in its command-owned `.jobserver-credit-<command>`
directory. A controller transport's temporary directory does not retain this journal.
Unknown pre-existing credit directories are held, never deleted/adopted. This is bookkeeping for the existing unit permit and
implicit slot; it creates no semaphore, runnable queue, pipe token or renewed
ordinal budget. Each root unit binds its original ordinal/scope and supervised
stdin record's normalized jobserver pipe identity. A nested request must match
that inherited identity before parent pause. Unknown/missing models or changed
pipe capabilities hold before lending. Root and nested metadata queries may
have no jobserver; that fact cannot authorize a nested implicit-slot loan.

The parent prepares one child loan, lends only after the original kernel freeze
observation, and the child enters that same credit at its existing exec stop.
Each command/parent/child loan keeps its original identity; ordinal reuse, a
second child, an unrelated scope and foreign journal contents refuse admission.
Original child cleanup returns the credit only when that child has no unresolved
loan. A pre-dispatch prepared loan can be aborted, while a lent/unknown subtree
stays fenced. Parent resume requires its owned, child-free state as well as the
separate signal, filesystem-transfer and cleanup gates. Original root cleanup
closes the credit; it does not expose another implicit pipe byte. Failed journal
acknowledgements replay only the same holder's known exact state. Lost/forked
holders cannot adopt it. All records keep false input/CI/shipping flags.

Thirteen real-file/pipe host cases cover four exhausted test semaphores and an
empty real pipe, nested same-credit chains, unchanged pipe flags/bytes, wrong
pipe/scope/model, duplicate/unknown loans, lost prepare/lend acknowledgements,
root closure and the original cleanup hook. That four-slot component experiment
is not the required four-parent pinned-Cargo test above. At `f1b30ea5`, all 99
guest cases passed in 94.25 seconds as UID1000: the previous 86 filesystem/signal
cases plus these 13 lineage cases. Original resource removal was verified.
All 557 Python tests and 34 focused managed Rust tests pass at that revision;
the subsequent managed-target journal placement also passes the 13 focused
credit tests and 34 Rust tests. The unchanged original Cargo replay remains
pending. Actual four-parent exhaustion/cancellation/reacquisition and checked
parent resume remain mandatory before this model can pass its integration gate.

At `8a8b307c`, the unchanged Cargo replay stayed red at the deliberate
unmaterialized-transfer hold in 103.27 seconds. Managed read-only inspection
verified the retained journal's binding, owner/mode, shared original credit and
kernel pipe across units 8–12. Independent comparison against the original
admission/cleanup records verified every scope and pause hash. The successful
leaf loan returned; interrupted ancestor loans 9–11 remain lent and their parents
stay fenced. The strict full-chain-return/root-closure verifier is red; those
states must not be reset or relabeled as returned to manufacture acceptance.
Observation: `/tmp/autoship-1703-credit-lineage-fenced-observation-1.json`.

A fourteenth real socket/FD regression reproduced acceptance of jobserver-pipe
aliases on each stdio stream. Such a stdin stager could consume token bytes;
stdout/stderr could fabricate them. Admission now rejects those kernel-inode
aliases before reading stdin, pausing a parent or preparing a credit loan.
The same test accepts distinct stdio and checks unchanged original pipe bytes.
At `092fa2da`, all 100 guest cases passed in 96.92 seconds as UID1000, with
original resource removal verified. The read-only model/pipe check now also
runs at authenticated receive, before stdin staging, so missing/substituted
models cannot drain a stream before refusal. A fifteenth host case verifies
that ordering without changing pipe bytes or the credit journal. The revised
101-case guest check at `4d99b193` passed as UID1000 in 295.73 seconds,
including managed admission/reconciliation, and verified original resource
removal. All 559 Python and 34 focused managed Rust tests pass. The first
101-case attempt at `3de923f4` failed before the suite because the count edit
also accidentally changed the expected UID; both original UID1000 assertions
were restored before this successful run. No role/resource criterion was waived.

The unchanged Cargo replay at `4d99b193` remains red at its deliberate transfer
hold in 104.26 seconds. Independent managed image inspection reverified original
exec/cleanup, every ancestor hold, the unchanged sealed before/after graph and
the managed credit journal's command, scope and pause bindings. Units 8–12 use
one original credit and pipe; the successful leaf's loan returned, interrupted
ancestor loans 9–11 remain fenced, and strict full-chain return/root closure
remains red. Observation:
`/tmp/autoship-1703-credit-lineage-integration-observation-2.json`.
These results close neither the real four-parent Cargo exhaustion gate nor
full-chain return/resume; no parent mutation or CI/merge authority is enabled.

The controller now takes read-only, instant jobserver/dispatcher observations
at the original post-freeze loan boundary. `FIONREAD` samples pending bytes;
normalization, inode and flag checks surround it without reading/writing a token
or changing flags. The original dispatcher's existing semaphore and supervisor
set are sampled without acquiring/releasing a permit. Each loan record binds
these observations, the same-pipe lent root ordinals and controller monotonic
time to its existing command/parent/child/pause identity. These are observations,
not exclusive-writer, complete credit or passing-CI proof. Five host regressions
cover empty/filled real pipes, unchanged bytes/flags, invalid bindings, ioctl
failure and observation of four exhausted original test slots.

A separate opt-in `live_shared_ci_four_parent_jobserver_progress` now runs four
real binary targets through the unchanged protected repository adapter using
actual pinned Cargo `--jobs 4`. It uses 2000 real functions per target and the
existing resource ceilings; no wrapper, filename exception, extra permit pool,
new execution mode or hand-constructed jobserver pipe substitutes for Cargo.
The initial build and repeated compatible build require original compiler counts
`[4,0]` and original Cargo cleanup. Its postmortem companion must verify the
root-owned pipe/slot/cohort records. The test does not initiate cancellation and
always leaves cancellation/reacquisition and full exhaustion acceptance false;
it is the initial execution stage of the still-open four-parent gate. A separate
controlled cancellation/reacquisition scenario remains mandatory; a successful
initial build alone cannot satisfy O4. The original V3 fixture, arguments,
counters, streams and acceptance assertions remain unchanged.

At `e4a3a55e`, the first actual four-target attempt deferred before guest/Cargo
execution after 300.26 seconds because a live production deployment/verification
held the managed lease; that attempt supplies no model evidence. The next
attempt ran the actual protected adapter in 108.34 seconds and remained red with
`InputCensusUnknown`. Managed read-only image inspection verified a shared empty
Cargo pipe, zero available original slots, four original supervisors and the
four lent root compiler units 5–8. All four nested root children progressed into
further admitted execs; four deepest children returned zero with original
cleanup. Two complete sealed ELF outputs (651616 bytes each) passed independent
header/size/hash checks; the other two parents were interrupted before sealing
completed. Every original parent still withheld its result, with no root
filesystem application/resume. The initial exhaustion/progress experiment is
positive; the actual build and full four-parent gate are red. Cancellation and
reacquisition were not exercised and remain unproved. Observation:
`/tmp/autoship-1703-four-parent-observation-2.json`.

The expanded component guest check at that revision passed all 106 cases as
UID1000 in 95.76 seconds, retaining the previous 101 cases and the new five.
Original resource subtree removal verified in both actual VM experiments.
All 564 Python and 34 focused managed Rust tests pass; routine Rust ignores 24
guest opt-ins, including the new actual Cargo test. All input/CI/shipping flags
remain false. These results do not replace the required writer/alias/W proof,
root transfer/resume or controlled cancellation/reacquisition acceptance.

Routing and cancellation must not wait for an execution permit held by the
request's paused ancestor. Containment is acyclic and recorded before dispatch.
Iterative traversal closes arbitrarily deep admitted chains within the existing
request, descriptor, byte and process resource limits; exhaustion holds rather
than raising those limits or inventing a renewed grant.

Ancestor cancellation, transport loss or deadline expiry revokes the whole
contained subtree. Forward original signals in order through the controller
lineage to the current executing image. Stop and positively drain children before
resuming a parent or releasing its outputs. Unknown teardown retains state and
the original R fence. A paused producer is never mistaken for a completed one.

### O5. Resolve staged lineage before CI authority

Store captures and their states under the existing command/executor ownership
root. A capture record has the original Binding, command, parent unit/scope,
contained call, initial view/provenance references, immutable inventory/digest,
pause evidence and an unresolved lineage reference. Commit preparation before
exposing the bytes; missing/torn/foreign/contradictory state refuses use.

An active command may execute a child against its own committed input capture.
Neither an interrupted command nor another command may adopt unresolved capture
lineage. A child's own original successful cleanup permits sealing provisional
child effects and returning them within that command while an ancestor capture
is pending. Keep these effects in a command-local provisional index; they cannot
enter the existing reusable publication index or be adopted by another command.
Same-command selector checks validate the committed immutable capture and its
live original ownership, not a future ancestor cleanup receipt. Existing reusable
artifact publication still requires successful original producer cleanup,
immutable output validation and all referenced lineage closed.

At parent completion, reconcile every contained call, original exit status and
cleanup receipt. Resolve available original lineage references without rewriting
capture-time facts; ancestor references may remain pending until that ancestor's
real completion. Successful effects can be sealed provisionally and returned to
the next waiting parent. Promote them through decision 9 into the reusable index
only after every referenced original lineage closes and full input/provenance
checks pass, preserving failed-origin restrictions from O3a. A
failed parent publishes no reusable outputs. Its stopped, attributed input
capture can still support a source-correct failing check when all original input
and cleanup evidence is complete; this is not reuse of failed producer output.
Unknown capture/source/runtime/cleanup evidence holds instead of charging repair.

Final passing CI requires all referenced capture lineages closed, complete input
proof and all required local coverage. This draft adds no final proof type or
required-version source. C's separately audited single contract must encode and
validate these relationships before V/M/baseline consumers receive a handoff.

### O6. Publish before accepting Cargo's cached decision

The current command's active generated/artifact view overlays its committed
provisional effects, in controller order, on the admitted immutable reusable
base. Other commands see only eligible closed reusable versions. Consumers bind
the exact visible effect/base version and original provenance, not just a path.

Commit provisional effects after their own original execution/cleanup and
transfer validation, before returning the contained result. That durable commit
advances the command-local view and schedules original selector revalidation,
including consumers Cargo skipped as fresh. The authoritative executor-command
state extension records the visible effect/version and its invalidation sequence
or an explicit pending-revalidation obligation in one metadata commit, after
effect payload sync. A crash cannot expose an effect without its obligation.
Missing/torn/unknown state holds; prepared-but-unreferenced payload is not active.
Persist affected units and transitive dependents before allowing a Cargo terminal
response. Unknown selectors/closure hold.

Later promotion after complete original lineage and input proof changes only
reuse eligibility, not content/view identity, and emits no second content-change
event. The provisional view is command-local execution state; promotion is not
its first visibility or invalidation boundary. Test crashes between payload sync,
view/obligation commit, revalidation, Cargo return and eventual promotion.

Selectors distinguish external admitted inputs from invocation-local captures.
A local capture is checked against its immutable historical bytes and original
same-command ownership, not its parent's later final output. Pending lineage is
permitted for these provisional execution checks only. External changes revalidate
the owning opaque unit's complete initial input closure, invalidate that logical
unit and dependents, and require a new contained capture when it executes. There
is no blanket exemption for captured upstream data. A producer's own publication
does not replace its earlier input retroactively with its later output; other
consumers of the replaced external version still revalidate. Test both an
intentional parent change after its child and an independent upstream change.

Nested precise compiler reuse across invocations still requires complete selected
input identity and original output provenance. A new capture ID alone cannot
force recompilation; matching names or content alone cannot establish metadata/
runtime equivalence. Opaque wrapper execution and underlying compiler execution
are separate facts. The unchanged original counter regression and all D8 reuse
requirements remain mandatory activation gates.

Intercept the original Cargo result before its caller receives a terminal
response. Drain that Cargo scope, contained calls and original handlers; retire
only the selected protected fingerprints through the existing durable intent and
recovery mechanism. Restart the exact same Cargo request with its captured stdin,
original streams, selection and remaining grant. The caller's Python/script body
does not restart. Previously emitted Cargo stdout remains original output; a
second pass appends its real output. At most four passes including the first.

After the required transfer, selector/invalidation and bounded Cargo-pass work,
return the original raw Cargo status provisionally to its still-live caller.
Pending worker/ancestor lineage alone does not prevent this execution response;
otherwise the caller could never finish and close it. This response grants no CI
or reusable-output authority. Revalidate final generated state and compiler
inputs after original worker cleanup before reporting a source-correct red or
green. A later invalidation cannot be repaired by rewriting the earlier raw
response or inventing successful coverage. Pending invalidations, unresolved
lineage or oscillation block final verdict/proof authorization. A red invalidated
by later publication is stale too. Preserve history, budgets, owner holds and
uncertain remote mutations.

## Crash and retirement

The existing command lease and R ownership are authoritative. Every pending
capture, pause and materialization belongs to that lease and its original scopes.
After controller death, never assume stopped processes stayed stopped or that
cleanup happened. Positive whole-VM/scope teardown and ownership proof from R are
required, but do not close an active C execution claim or fabricate original unit
cleanup. Joint C/R interrupted-owner recovery is a prerequisite to implement,
not an existing completed path. Bind recovery to the original Binding, claim,
command, physical-cache identity and R settlement evidence. C inventories each
capture/transfer intent and durably marks the old claim abandoned/non-reusable;
R retains its fence until that disposition is acknowledged. A new claim may
then reconstruct safely, never adopt unresolved lineage. Replayed recovery uses
the same old-claim/recovery identity and resumes interrupted transitions rather
than creating another grant. Abandonment preserves source/audit history and
cannot satisfy any producer's original successful-cleanup requirement. Test
crashes at both C/R acknowledgements before step 3's crash exits or handoff.
No manual fence deletion or raw cache wipe is recovery.

Captures and publications remain until every original consumer is positively
drained. Retirement verifies original identities and references under the existing
lease/budget; partial removal is recoverable or explicitly non-reusable. Completing
this retention path is required before activation and is not deferred to the
owner. No parallel allocator, accounting store or competing teardown authority.
Before exposing a capture/publication or inherited inode capability, C enrolls
its object/reference and original consumer scope in the existing executor-owned
records. Reuse enrolls its own original scope too. Under the same ownership lock,
retirement fences new enrollment and supplies R the complete original-consumer
set and verified drain receipts. Unknown references or receipts retain the fence.
Before unlinking payload, durably commit an exact retiring disposition including
identities, inventory, drained references and removal progress, and remove its
reuse eligibility. Replay validates recorded removals or holds. Completion marks
payload removed/non-reusable while preserving original audit/provenance metadata.
C recovery must recognize this authenticated disposition; arbitrary missing
payload is still corruption. Publication/checkpoint readers must stop requiring
retired historical payloads as live reusable objects. This C/R integration is
required before activation; the existing producer-only reclamation API alone
does not establish complete consumer drainage.

## Pending invalidation and fingerprint recovery

C owns the durable revalidation obligations under the existing executor owner.
Keep the monotonic history, but assign each obligation an invalidation sequence,
logical unit, input/publication versions, dependent edges and protected
fingerprints. A completion satisfies only that exact sequence after original
execution/cleanup and current selector validation; compare against the latest
sequence so a stale completion cannot clear a newer change. A superseding
obligation records its predecessor and why it covers it, never deletes history.
Green requires all relevant obligations satisfied. An attested current upstream
failure may terminate red with unexecuted dependents marked blocked by that exact
failed version; this is neither passing coverage nor permission to reuse failed
outputs. A later input change invalidates that red witness too. Unknown evidence
stays pending and holds. Test crash/replay, publication racing discharge, later
invalidation, failed-upstream dependents and four-pass exhaustion.

Fingerprint retirement must implement intent replay; current prepared/retired
diagnostics alone do not supply it. Bind the intent to the original protected
target/claim, selected identities and before-inventory. Record durable progress
for each selected removal and sync the metadata directories before completion.
Replay accepts only the recorded original identity or its authenticated completed
removal, preserves unrelated entries and holds on a new/foreign/contradictory
entry. Do not demand the full original inventory after a recorded partial removal,
or infer successful retirement from absence alone. Test every removal/commit
boundary before restarting the original Cargo grant.

## Build order and behavioral exits

1. Preserve the original native red and all current native passes. Add actual
   kernel tests for namespace/credential attribution, threads/fork fallback,
   pause census, hardlink/mmap writers, cancellation and uncertain cleanup.
2. Test scoped unit transport, other-unit credential/namespace refusals, nested
   request limits, live-parent credit transfer and bounded cancellation without
   a new resource grant. Keep original V3 streams/signals/jobserver cases.
3. Test a real wrapper and its nested compiler: actual wrapper side effects,
   staged stdin, sealed input capture, child output materialization, original
   parent/child cleanup and failed-parent non-publication. Crash each boundary;
   unresolved lineage cannot be adopted or converted to a passing proof.
4. Connect real generated publication, selector invalidations, protected
   fingerprint retirement and original Cargo restart. Preserve unrelated units
   and original immutable histories. Run oscillation and fourth-pass exhaustion.
5. Enable the supported execution mechanism at the existing production adapter
   entry, rather than a filename/env-selected regression shortcut. Incomplete
   input/provenance remains a typed hold at the authorization boundary. Run the
   unchanged original in-command regression: warm two consumer compiler calls,
   unchanged zero calls, publication after fresh decision, genuine E0761 and
   nonzero consumer calls/exit, unchanged old bytes/mtimes and direct control.
6. Complete runtime/input/trusted-seed and actual-plan capacity gates, finalize
   and independently audit the single C proof/API, then test all real consumers
   and normal updater/writer cutover. Only then proceed to the original live
   CLI/Discord and soak/confirmed-merge acceptance.

## Open threads and accepted risks

C owns implementation of O1–O6 and invalidation obligations; R owns original ownership/teardown primitives and
retirement integration. These are implementation gates, not requests for changed
product requirements. Kernel pause/namespace feasibility, original signal/stdio
behavior and required-plan compatibility must be established by steps 1–3 before
the mechanism can be handed off. Failure means revising this mechanism, not
waiving D1–D8 or editing the original fixture to pass.

The paused-parent input model rejects concurrent mutable IPC. Its carrying cost
is explicit prerequisite holds until the actual required plan demonstrates this
model is supported; activation is blocked meanwhile. Cross-reconstruction
workspace optimization remains separately deferred under D8; within-job compatible
reuse and verified external dependency reuse remain mandatory.

The following load-bearing choices remain parked with C; they are not waived:

* Before step 2/3 handoff, establish unfrozen signal ingress and its ordered
  pause/resume barrier for every admitted signal target. The existing proxy
  handler cannot receive while its UID is stopped. Define the controller routing
  epoch/acknowledgement and late/duplicate/signal-at-terminal behavior; prove the
  unchanged signal cases plus a genuinely stopped nested proxy. Unknown ingress
  cannot be described as preserved signal behavior.
  The root controller now supplies `StagedParentSignals` for the input-staging
  window. It marks the original request before freezing, queues root-ingress
  frames and cancels that staging attempt, then serializes verified thaw and
  queued sends. Wrong/repeated request identities cannot resume another call;
  failed thaw keeps the queue, late sends are explicit and the original
  4096-frame limit remains. Standard kernel signal coalescing is unchanged;
  per-frame userspace handler delivery/order is not promised. Host kernel tests
  exercise a stopped receiver, thaw races and failure, ordinary/late delivery,
  permanent cancellation and pre-auth busy-socket revocation. This is not an
  active-child router at that revision. Actual contained-child routing, stopped nested-proxy
  guest signal cases, terminal races and checked child drain before parent
  resume remain this thread's gates. That staging barrier alone enables no child execution grant.
  At `b1bcede6`, the unchanged pinned-guest replay completed in 98.00 seconds
  and reached the held child grant. Its original capture/pause/request bindings,
  verified thaw and all four files / 21 entries / 64,832 bytes passed the same
  independent read-only image validation. The staging barrier settled with zero
  received, queued, delivered or late frames and no revocation. This checks
  integration without injecting a signal into the guest's frozen compiler;
  actual stopped-recipient signal behavior is covered by the eight host kernel
  and socket cases, not by this zero-frame guest observation. All 431 Python
  tests and 34 focused Rust tests pass; full CI and shipping authority stay false.
  The controller now links signal gates at the original supervisor's exec-stop
  acknowledgement, before releasing the unchanged exec grant. One lock owned by
  the original command serializes that transition and ingress; a signal arriving
  during the grant waits for its outcome. Iterative routing follows the current
  active child chain through stopped ancestors to its pidfd. No stopped proxy
  has to forward a frame. Wrong epochs, duplicate/multiple-parent links, cycles,
  another command's lock and the unchanged 4096 bound refuse admission. Unknown
  grant/delivery outcomes revoke the route and are never retried as fresh sends.
  Ancestor cancellation synchronously revokes descendants before another grant;
  its missing propagation was first reproduced by a failing regression.

  A signal after leaf exit remains late at that leaf until the original
  supervisor validates its raw status and all-descendants-reaped marker. Only
  then does the cleanup hook detach the signal target, before its transport
  closes the owned pidfd. Dead-pidfd observation alone grants no cleanup proof.
  Resume refuses any still-active child, and staging/transfer signals still
  queue and revoke the current attempt. Thaw, transfer completion, jobserver
  credit return and terminal-result delivery remain separate unfinished gates.
  Thirteen host kernel cases cover actual stopped parents, two stopped ancestors,
  ordered leaf signals, grant races/failure, late delivery, live-child drain/
  resume refusal, original hook binding and ancestor cancellation. All 543 Python
  and 34 focused managed Rust tests pass; routine Rust ignores 23 guest opt-ins.
  The expanded guest suite retains the previous 63 filesystem cases and adds
  the nine existing staging and 13 active-signal cases; execution is pending.
  The unchanged Cargo protocol also needs replay to verify the actual root
  hooks, followed by injected stopped-proxy/terminal signals at those hooks.
  These host/component results establish no final signal, CI or merge proof.
  At `ee14bc76`, the 85-case guest suite passed in 94.86 seconds as UID1000
  and positively removed its original resource subtree. The unchanged full Cargo
  replay remained red at the deliberate transfer hold in 104.27 seconds, but
  reached all four contained grants and the successful leaf. Independent managed
  image inspection reverified seven before/old files (64832 bytes), eight after
  files (537776 bytes), graph/intents and original leaf cleanup. The leaf's signal
  target was detached; no signals were injected in that Cargo replay.

  A fourteenth real-process regression reproduced cancellation raising after an
  exited child's gate had already closed. In the Cargo replay that interrupted
  outer ancestor hold records. Revocation now traverses those known closed gate
  objects without using their closed pidfds; it keeps the active hold and blocks
  signal dispatch/resume. Only original verified cleanup can release it. The
  revised 86-case guest check at `698d987d` passed in 96.25 seconds as UID1000,
  including all 14 active-signal cases, and verified original resource removal.
  All 544 Python and 34 focused managed Rust tests pass. Receipt:
  `/tmp/augmentagent-ci-active-signals-20261104070215/receipt.json`.

  The unchanged Cargo replay at that revision remained red at its deliberate
  transfer hold in 102.12 seconds. Managed read-only image validation now verifies
  all four ancestor hold/revocation records (requests 9–12), each original exec
  observation and all-descendants-reaped marker, the successful leaf's detached
  signal target, the unchanged sealed before/old/after graph and VM quiescence.
  The three aborted ancestors retain raw -9 dispositions; the leaf retains raw
  zero. No active signal was injected into these actual Cargo hooks. The proof
  `/tmp/autoship-1703-active-signals-integration-payload-proof-2.json` records these
  observations without claiming parent resume, materialization or CI authority.
* Before producer/restart integration acceptance or mechanism handoff, settle the
  complete-initial-closure rule for successful sibling output effects. The
  unchanged fixture appends counter/log data in the same generated namespace as
  real compiler inputs. A blanket namespace change must not create endless
  sibling invalidation; a blanket capture exemption must not hide a true source
  or conditional-wrapper input change. Require the original counter regression
  and adversarial actual-input mutations; no filename, env-counter or fixture
  exception resolves this choice.
* Before supported-adapter/final-C authority, finish the trusted kernel-policy
  disposition observer. Intentional admitted runtime denials, including clone3's
  pinned fallback, differ from an unsupported route that makes execution
  incomplete. Unknown ABI/exec/policy coverage needs bound controller evidence,
  checked original cleanup and the existing typed hold, never an inference from
  application errno/signal alone. The new raw-exec controller tests cover part of
  this boundary; stricter stacked filters and the remaining runtime/input policy
  still require evidence. All real consumers must receive the final C/S mapping.

The private diagnostic replay at `1bd0b1d0` now reaches the original failed child
without consuming its output pipe: GCC 13 reports that starting
`/usr/libexec/gcc/x86_64-linux-gnu/13/collect2` fails at `vfork` with ENOSYS.
The original child exits 1, original cleanup is checked, its jobserver credit is
returned, and parent resume remains held. Original host VM quiescence is verified.
The unit policy intentionally rejects raw `vfork`, `CLONE_VFORK` and unsupported
separate-process shared address spaces because the retained status proxy cannot
release that shared address space at the delegated tool's exec boundary. CPython's
ordinary fork fallback does not establish that GCC has one. This is a supported-
adapter compatibility gate owned by C, with R owning the original process/exec
and cleanup boundary. Before final-C authority/handoff, admit an execution path
for the unchanged pinned compiler invocation and prove its parent release,
shared-memory/FD/stdio, signal, exit-status, jobserver and original-scope behavior.
Do not enable raw exec/shared-mm creation, swap the fixture compiler, alter its
argv/environment/counters or treat a preload-only path as kernel mediation to
make this regression pass. The compiler error is an actual private observation,
not the trusted kernel-policy disposition proof still required above. The
separate copy-back, closure and projection gates remain even after this runtime
compatibility exit is implemented. No source/CI/shipping authority is established
by these diagnostic probes.

The selected implicit-credit jobserver model also remains a feasibility gate
until its actual pinned-tool saturation/cancellation tests pass. The joint C/R
abandonment, consumer enrollment, retirement dispositions and fingerprint replay
above are work to implement, not claims about today's code. These gates block
mechanism handoff and activation; experiments in steps 1–3 remain unauthoritative.
