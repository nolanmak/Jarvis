# Native durable balance and replay checkpoint

Working C0 amendment under #1918/#1920/#1917/#1703. This specifies the balance
representation and its validation boundary. It does not close the original
handoff, growth/profile, registry/anchor identity, migration or execution-grant
contracts, and authorizes no production schema change or native enrollment.

## Decisions

B1. Add the four capacity tables and triggers in
[the executable proposed schema](probes/native-storage-ledger.sql) to the existing
root runtime-retirement.sqlite; preserve retired_owner unchanged. No balances
are copied into workflow state or the global root-membership registry. The
singleton is version 1 of this balance schema, distinct from the monotonic
storage protocol floor 2 and existing native policy versions. New tables are
STRICT. All numeric inputs must first be real integers (not booleans, floats or
numeric strings), 0..2^63-1. SQL affinity may coerce inputs, so STRICT alone is
not the input validator. Sums use checked Rust arithmetic, never SQL total(),
REAL promotion or saturating arithmetic. Any overflow is unknown, not capacity.

B2. Allocation, operation, session, launch and filesystem identifiers are exact
16-byte UUID values, with no string aliases. Digests are exactly 32 bytes.
Filesystem UUIDs are the persisted O8 identity; held mount/device/path identity
and actual original writer proof remain separately required. Detect any
allocation/operation/launch collision across registered roots as unknown; a
local PRIMARY KEY does not guarantee global uniqueness. Session UUIDs have
one global (root UUID, retained allocation UUID) binding: repeated operations
for that binding are valid; assignment to another root/retained row is unknown.
Each operation binds
one root, session, fresh launch, action, original binding bytes, policy/demand
revision digests and replay digest. Origin bytes (1..65536) come from the
original owner protocol, remain immutable, and grant nothing by being stored.
Their producer/qualified interpretation remains an explicit handoff gate.

B3. A create has exactly one retained allocation and one active-growth
allocation. A resume has exactly one active-growth allocation and references
its existing session's retained allocation in the same root. At new resume
admission that row must be held; inventory of historical settled operations
instead requires the original matching retained tombstone, which may later be
released. A released historical launch does not require live retained capacity.
The unique retained index includes released tombstones: session UUIDs are never
reused. Different operation UUIDs cannot create another retained allocation for
the same session. Each allocation carries 1..64 demand rows, one per filesystem.
Components on the same filesystem are summed before encoding; never duplicate
the same allocation/filesystem row. Counts and full operation shapes are checked
before commit and on every inventory read; an operation with missing/extra
allocation or demand rows is unknown, not a partial balance.
A retained allocation has exactly one demand row, on capacity_meta's immutable
state_filesystem_uuid, with exactly 1073741824 bytes. That persisted identity
must match the qualified O8 observation of the actual native state root; it
cannot redirect the charge to a larger unrelated disk. Other retained rows are
illegal. Its producer inode counts and active peaks must be explicitly
known, not fabricated as zero. Every launch allocation must be released before
retained may even enter
release-pending; that transition still charges the full retained promise. Both
this ordering and terminal release preserve all historical create/resume rows.
Resume needs original ownership proof as well.

B4. Replay uses the original operation UUID and exact immutable payload. A
repeated UUID with any changed payload refuses; a repeated matching operation
returns its existing rows without mutation or physical write permission. A
released allocation stays released and cannot restart. A new operation leaves
all earlier nonreleased balances charged. The canonical replay preimage is UTF-8
JSON, keys sorted by ASCII name at every object, no whitespace, only ASCII
strings and unsigned decimal integer values (no bool/float/null). Its fields:
version=1, root, operation, session, launch, action, origin_sha, policy_sha,
demand_sha, allocations. UUID/digest strings are lowercase hexadecimal without
hyphens; origin_sha is SHA-256 of exact origin bytes. allocations is ordered
retained then active-growth, each object contains id, kind, demands. demands
is ordered by raw filesystem UUID and each object contains filesystem, bytes,
inodes. Replay SHA-256 hashes these exact canonical bytes. Recompute from the
stored rows and compare on read. Phase, release proof digest and inventory
revision are excluded so settlement does not change original intent.

B5. Every ordinary allocation INSERT starts pending with no release_sha; a
DDL insertion guard enforces it. Qualified imports must use the same pending
and guarded transition path, not insert settled rows to bypass original proof.
The migration/original imported-proof producer remains a separate gate.
pending, held and release-pending all count their full demand; released
alone counts zero. Allowed edges are pending->held, pending->release-pending,
held->release-pending and release-pending->released. Exact no-op updates are
allowed; no backward/reopen edge or deletion exists. Private mutation uses
plain INSERT and explicit checked UPDATE, never REPLACE/UPSERT/IGNORE conflict
shortcuts. Collision-aborting BEFORE INSERT guards cover every unique identity
in all four tables, including alternate launch and retained-session keys. Thus
INSERT OR REPLACE cannot bypass immutable UPDATE guards by implicitly deleting
an old row; this protection does not depend on recursive_triggers. Matching
idempotent replay reads existing rows before any INSERT attempt. The schema's release_sha
is only a durable correlation to original proof already consumed by the trusted
release operation. Arbitrary 32-byte input is NOT release authority; SQL triggers
cannot validate native custody. The private writer API must require the opaque
original qualified retirement/cleanup capability before the last edge, after
precisely owned deletion, parent sync and retirement publication. A read-only
inventory, copied digest/row or generic resource completion cannot produce it.
The producer/API remains a blocking dependency; this checkpoint does not
substitute a test-only permission or deserializable proof.

B6. Capacity mutation holds native state then common Admission then the root DB
transaction, as S6 requires. Use busy_timeout=0, foreign_keys=ON with readback,
synchronous=FULL with readback; do not silently inherit connection defaults.
Use the root's checked existing file/parent identity and reject unknown schema
objects/definitions. Proposed supported ledger journal mode is DELETE, installed
only by the qualified migration under exclusive original handoff; readers do
not change journal mode or create sidecars. Preserve identity checks before and
after the operation. A complete graph insert plus one checked revision increment
is a single transaction; every later capacity-changing state update likewise
increments revision once. CAS on expected revision and checked +1 prevent stale
or overflowing publication. No raw DB connection escapes the private writer API.
Read-only inventory opens only existing DBs, query_only=ON, busy_timeout=0,
checks all graph/replay constraints and schema revision in one read transaction,
then revalidates identities. Busy, malformed, missing or changed evidence is
unknown. Common admission acquires no native state lock while scanning roots.
A journal requiring recovery that a read-only connection cannot perform remains
unknown until the original qualified recovery path settles it.

B7. Atomic SQL does not replace cross-store ordering: membership is synced
before charge, complete pending intent is committed/synced before physical
growth, and crash or lost acknowledgement preserves full charges. Directory
sync at DB creation and the existing original publication/release sequence
remain required. Snapshot rollback cannot restore an older balance revision or
erase tombstones. Exact membership/anchor, supported migration/snapshot format,
proof API and growth enforcement remain C0 gates before production handoff.
The bounded control-plane growth producer must also cover registry, SQLite
intent/rollback-journal writes and retained tombstones. The intent record's own
physical writes necessarily precede its commit; they need a separately qualified
pre-existing control-plane reservation/bootstrap allowance, not an assumed zero
peak or authority from the uncommitted row. Exact coverage/transfer/recovery is
still a load-bearing growth/bootstrap gate. This checkpoint grants no write
permission for that unsolved pre-intent interval.

## Evidence and remaining work

The committed disposable [SQLite probe](probes/native_storage_ledger.py) exercises
real DDL, transitions, immutable rows, checked sums, graph/replay validation,
commit/reopen and child-process crashes. Run `python3
docs/autoship/probes/native_storage_ledger.py` from the repository root. On this
host SQLite 3.45.1 passes all 40 predicates; both crash-child execution markers
are verified. This is not production Rust reader/writer evidence.
It must distinguish precommit crash (no published intent, hence no writer
permitted) from postcommit crash (full pending charge survives). No VM, model,
production DB, registry, root permissions or capacity policy may be changed.
Raw SQL release tests demonstrate shape checks only, never original-proof
consumption or deployed admission. Linux filesystem identity/quota/inode and
kernel power-loss guarantees require their separate integration evidence.

Fresh-reader, conversation/source comparison and all 16 mapped seams were
reviewed. Six findings were corrected/rechecked: pending-only insertion,
retained state-filesystem placement, admission versus historical resume validity,
retained release ordering, global session binding and REPLACE bypass. No
remaining checkpoint finding was retained. Pre-intent control-plane reservation
was explicitly added as an unresolved growth/bootstrap gate. This is not full
C0 handoff. Full C0/C2 and #1703 productive ingress, normal rollout and live
acceptance stay incomplete.
SQLite mechanics: [STRICT tables](https://www.sqlite.org/stricttables.html),
[foreign keys](https://www.sqlite.org/foreignkeys.html),
[connection pragmas](https://www.sqlite.org/pragma.html),
[conflict/replacement semantics](https://www.sqlite.org/lang_conflict.html).
