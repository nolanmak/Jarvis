# Bounded native boot archive component

Scoped C0 producer component #1932 under #1918/#1917/#1703. Full active-growth,
physical filesystem/inode/quota, original source/resource/input custody, durable
accounting/migration and productive owner ingress remain blocked. This component
bounds one real producer; it does not activate capacity or model execution.

I1. The supported native boot archive has an explicit 128 MiB uncompressed newc
limit, including every header, UTF-8 name, NUL, alignment and trailer. This
profile sets one total ceiling, not separate 64 MiB partitions. A normal
64 MiB home seed leaves the remainder for bootstrap/encoding; final seed
aggregation itself is fixed separately in #1926/#1927 and is not assumed
present on baseline main (where auth selection can produce 65 MiB). It is a producer capability, not a machine-wide pool.
No environment/policy override or truncation is added. Too-large input refuses
before model release; guest RAM, build demand and the independent MAX_HOME limit
remain unchanged. No claim that all runtime configurations previously accepted
will fit this newly bounded supported profile.

I2. Native boot entry construction charges the encoded size of every entry,
reserving the trailer throughout. Busybox/modules and trusted guest/gateway
script inputs use bounded held-descriptor reads against remaining archive
payload capacity, in chunks <=64 KiB with at most one excess-byte lookahead.
Require bytes read equal initial size and unchanged held regular-file identity,
size, mode, owner, link count, mtime and ctime before/after reading. Reject
short read, observed shrink or growth,
replacement observed at the descriptor or nonregular sources without blocking.
Preserve existing source symlink resolution semantics; these trusted declared
runtime paths are not a new read grant. Loaded bytes form the snapshot; no
inode/stat observation is claimed to prevent every concurrent same-length write.
Already supplied seed/certificate/job bytes are charged on append. This does
not prove their entire upstream loading/memory peak, credential refresh custody
or whole-launch growth. Missing/unreadable/oversized inputs fail without public
path/content disclosure.

I3. Extend existing codex-build-vm.py initrd with optional max_bytes, preserving
all existing callers when absent. When present require positive real integer
(not bool); compute each actual next encoded archive size before extending the
bytearray and reject an excess, including trailer. The returned native gzip
image has a second 129 MiB compressed-data cap checked before opening output.
The compressed cap is an independent enforced bound, not an assumed gzip ratio
or proof of aggregate host memory. Native always supplies the 128 MiB limit.
Existing shared-CI callers remain behavior-compatible without the new argument.

I4. Native output is created exclusively, owner-private, only after archive
construction/compression and both limits pass. Never truncate an existing path.
On a later write/fsync failure retain normal original temporary-directory cleanup
and uncertainty behavior; do not delete unknown existing destinations or add a
retirement/release shortcut. No gzip image is created on preflight refusal.
The bounded compressed file is logical data only; ext4 metadata/allocation,
initrd buffers, temp directory accumulation and whole-launch peak remain gates.

I5. Native main passes script paths into the bounded reader rather than calling
unbounded read_bytes before the budget. Keep gateway provisioning, credentials,
job payload, QEMU command, native supervision/model/input release and checked
cleanup protocols otherwise unchanged. Preparing archive bytes grants no
source custody, body/model/input permission or shipping credit.

## Test-first exits and evidence

- Define tests first through existing boot_entries/initrd paths: native oversized
busybox/module/aggregate seed+bootstrap must refuse; legacy baseline's absent
new limit is additive-contract RED, not a falsely claimed old enforced-limit bug.
- Exact encoded boundary succeeds, one-byte smaller budget refuses; compare
real decompressed gzip length against independent newc accounting including UTF-8
names and alignment. Exercise every producer category, huge sparse regular
source, nonregular/FIFO refusal, changed/growing read and compression/output
refusal. Use small patched limits for most fixtures; one real 128 MiB+1 sparse
source must refuse before reading its contents on the fixed implementation.
- Verify existing destination bytes survive, oversized output is not created,
missing shared cap preserves old initrd behavior, and the native main entry
point invokes the mandatory bounded path without a VM/model launch.
- Run native-provider, provisioning and relevant shared VM unit suites, source/
whitespace checks, required exact-head hosted CI and independent code approval.
No production credentials, VM, policy, service or retained session changes.

Fresh-reader and all 17 mapped seams were checked against unchanged main
416f392b. The seed-premise and observed shrink/short-read findings were
corrected/rechecked. This explicitly permits only #1932's bounded producer
implementation while full C0 stays open. Its whole-contract audit and complete
#1703 acceptance cannot be replaced by these tests. First four additive-contract
RED cases are recorded: three expected refusal assertions fail and one new
encoder argument is absent. No implementation pass or old-limit regression
is claimed by this baseline.


Implementation [PR #1933](https://github.com/nolanmak/Jarvis/pull/1933), source
c60a7ae1, passes all 13 new component cases and existing native/provider/shared
VM/CI-contract suites: 92 passed, six existing KVM integration cases skipped
because JARVIS_TEST_VM_CONFIG was unselected. Real sparse overflow refuses before
payload reads; FIFO, observed mutation/shrink/short EOF, compressed prewrite
refusal, exclusive 0600 output, fsync failure and actual main routing are covered.
The test suite is registered in hosted bridge-suites. CI-contract verification
and whitespace pass. Hosted exact-head CI, independent approval and normal
rollout remain pending; no live VM/model or capacity activation is proved.
