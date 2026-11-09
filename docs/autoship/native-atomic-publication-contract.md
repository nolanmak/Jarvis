# Native publication storage component

Scoped producer repair #1934 under #1918/#1917/#1703. The baseline atomic helper leaves
a new NamedTemporaryFile(delete=False) after every file-sync/rename failure.
Four failed 64 KiB publications reproduced four retained files (262144 bytes),
with the old destination preserved. This is a real existing failure leak, not
proof that native durable accounting or execution is restored.

A1. One original native helper process has one private publisher instance,
created once at module import. Its lock serializes the whole synchronous publish
operation across main/gateway threads. No callback, network, await or native
metadata visitor runs under this lock. There is no public reset, environment
switch, automatic directory sweep or retry that clears uncertainty. Python
trusted controller code remains the authority boundary, not object secrecy
against arbitrary host code. A new process is not a cleanup grant: original
native handoff/release and durable charges remain required independently.

A2. Each payload must be bytes of at most MAX_FRAME (2 MiB), checked before
creating/opening mutation files. This bounds one control-plane artifact, not
all metadata of a launch or every caller's stricter credential read limit.
Oversize refuses without truncation. Validate/retain the actual existing
owner-private parent directory through an O_DIRECTORY/O_NOFOLLOW descriptor;
create one unique 0600/O_EXCL/O_CLOEXEC temporary file by relative dir_fd.
All replace/stat/unlink operations use that same parent FD. Recheck the parent's
original current pathname/held identity before publication and after sync;
replacement or changed privacy is uncertainty, never authority to target a new
parent. Source callers retain their original target/credential authorization;
the helper gains no permission from a name or payload.

A3. Handle short writes until every byte is written (zero/invalid progress refuses);
flush/fsync the original temporary file, atomically replace the target
through the held directory, then fsync the parent before reporting success.
Never undo a possibly completed replacement or delete the destination on error.
If replace returns successfully but parent sync fails, report the original
failure and latch uncertainty before another publication. If replace raises,
known nonpublication requires the exact still-held single-link source entry
still bound to its original name/parent. Otherwise the attempted replacement
is uncertain, including real replacement followed by a lost acknowledgment;
never infer nonpublication from destination absence or its later digest. Saved JSON/digest,
current absence or successful later stat does not erase it in this process.
No auth eligibility, identity, scope, grant or renewal/replay rule is relaxed.
A separate pre-existing caller issue can serialize refreshed credentials above
their 1 MiB read limit while under MAX_FRAME; that round-trip bound remains a
C0 producer/auth gate, not a guarantee from this generic cap.

A4. If failure precedes confirmed replacement, cleanup may unlink only the
unpublished temporary name while the original nonexported file descriptor remains
held, after checking that entry is a regular single-link owner-private file
with the same device/inode as that descriptor. Other, replaced, linked, missing
or unknown entries are preserved. Sync the held parent after owned cleanup.
Cleanup errors never mask the original publication failure. If cleanup identity,
unlink, parent sync or parent-path correlation cannot be proved, latch uncertainty.
This is creator-owned synchronous temporary cleanup, not all-writer custody,
native retirement, quota release or deletion permission from PID/age/du.

The proof assumes the trusted writer's original owner-private namespace and no
adversarial same-UID host code. Dirfd/identity checks detect observed replacement;
they are not a kernel-wide exclusion claim against all concurrent same-UID
namespace mutations. That stronger custody remains a separate C0/owner gate.

A5. After uncertainty, the original publisher refuses every further mutation
before opening a new file, even to another target. It may not allocate another
temporary file after an unresolved cleanup/publication outcome. A known failed
prepublication attempt whose exact temporary cleanup and parent sync succeeded
may be attempted again by the existing caller. This grants no upstream OAuth
retry, uncharged model call, new body or reuse of released native authority.
The synchronous original error remains the first result; later refusals have a
fixed path/content-free AtomicPublicationUncertain error, an OSError subclass.
main's failed-gateway observation is best effort: any diagnostic publication
failure must preserve the primary exception. The outer startup diagnostic also
catches a refused/failed receipt write, emits only fixed category/errno and
returns 126 without a chained traceback. Refusal can prevent complete.json or
other diagnostic publication; neither is successful native completion. A caller can still read/reconcile through its
existing original protocols; no new recovery/debt-clearing API is introduced.

A6. Crashes can leave one bounded temporary publication per original helper
process; this component does not clean that original process's leftovers from
another process or release their native charge. Payload cap, one serialized
publisher, uncertainty latch and conservative external accounting remain distinct.
Original proof, root membership, complete growth/inode/quota/metadata peaks,
control-plane bootstrap allowance, migration and productive handoff remain open.
No live credentials, services, policies, permissions or native sessions change.

## Test-first and delivery

Use new fixtures through actual atomic entry point. Reproduce repeated failed
sync/rename file leakage on unchanged main. Check success/private creation,
prepublication refusal, exact/oversize payload, short/partial write/flush/file
sync/replace errors, owned cleanup, cleanup refusal, replaced/missing/hardlinked
temp entries, parent substitution, rename-with-lost-ack and post-replace dir-sync
failure. Test the actual main diagnostic branch and outer startup entry with
a latched publisher; original errors and fixed diagnostics must survive.
Original exceptions/destinations and uncertain artifacts remain intact;
further writes create no new files after uncertainty. Exercise concurrent callers,
then existing native-provider/refresh/gateway/IO/provisioning suites and required
hosted CI/independent approval. No VM/model or production state is used.

Fresh-reader and all 13 mapped seams were checked against main 416f392b.
The first-error/diagnostic, outer refusal compatibility, observable rename and
credential-bound assurance findings were corrected/rechecked, with explicit
short-write rules. This permits only #1934's bounded producer implementation;
contract review is not independent code approval. Full C0/C2 and #1703 productive ingress,
normal rollout, live CLI/Discord and shipment/fault/soak acceptance remain open.
