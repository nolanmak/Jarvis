# Closed hard-link ownership for legacy incremental retirement

Status: proposed R amendment; implementation and independent review required.
Binding requirements: [#1807](https://github.com/nolanmak/Jarvis/issues/1807),
within [epic #1703](https://github.com/nolanmak/Jarvis/issues/1703).
This component does not narrow epic #1703 or #1807 D1–D8. Source proof, merge
admission/dispatch, normal main/updater rollout, original workflows, Discord
commands/sessions/updates, and the full unattended/soak/merge acceptance remain open.

Current implementation: opt-in mechanism and same-base native capacity comparison
passed at `c88829e3`. The normal-worker opt-in and retained-scope reporting carrier
are implemented locally and awaiting managed Rust/native validation and review.
The prior 57-seam review is complete; the changed reporting interfaces require
review against this revision. This component is not ready for production handoff.

## Evidence and premise

The read-only production-image rehearsal at executable `6bf753bc` passed in
158.46 seconds. Kernel descriptors were checked read-only before guest execution;
the entire original raw image had identical before/after identity and SHA-256.
The temporary view preserved 45,834 protected files / 4,167,370,136 logical bytes.
Version-1 retirement acknowledged four sessions but increased worker-available
space only from 20,738,048 to 53,387,264 bytes. This is insufficient restoration.

An independently admitted read-only census found 192 matching incremental
sessions containing 8,845,245,835 logical bytes (not physical reclaimable bytes).
Five sessions had unrecognized file names. Samples show normal shared object
inodes with link counts 2 and 52. Counting path bytes cannot predict freed space.
Evidence: `/tmp/autoship-1703-production-retention-diagnosis-1.log` and
`/tmp/augmentagent-ci-recovery-preview-20261103210041/receipt.json`.

The implementation's same-base comparison at `c88829e3` subsequently passed in
254.30 seconds total. Starting from 20,660,224 available bytes, strict selection
retired four sessions and ended at 53,309,440 bytes; closed-link selection retired
32 sessions and ended at 2,124,406,784 bytes. Both commands preserved the same
46,346-file / 4,167,370,136-logical-byte protected inventory and original base
identity/SHA. Receipt:
`/tmp/augmentagent-ci-recovery-comparison-20261103215444/receipt.json`.
This is the D9 mechanism benefit, not the C/R application working-set sufficiency
gate or production activation.

## Decisions and ownership

D1. Extend only the legacy cleanup controller in `ci-cache-generation.py` and its
existing pre-worker caller. Host admission must still establish the exact
registered physical target and original inherited flock descriptions, reconcile
prior cleanup holds, and exclude other users of the image. The fresh guest runs
cleanup before spawning workers. Session locks are additional exclusion, not a
substitute for those structural barriers. Unknown writers retain the cleanup hold.

D2. A link is eligible only after a complete, bounded census proves every endpoint
of that inode lies in recognized, safe, unlocked sessions within the same legacy
incremental unit. Use actual `(st_dev, st_ino)` and `st_nlink`, not bytes, names or
UID alone. Acquire all participating session locks and retain them through intent,
move and removal. Validate worker UID/GID, device, safe permissions and no xattrs,
symlinks/special files, unknown entries or unsupported directories in each session.
Missing/busy/unsafe peers are excluded; if this leaves any endpoint unaccounted,
retain the selected session. Unknown sessions are not retired to close a census.

D3. For each selected linked file, the number of distinct verified endpoints must
equal its actual link count. No endpoint may escape the same unit. A file with a
foreign alias (including a compiled artifact, fingerprint, source or unknown path)
retains the entire selected session. Eligibility is all-or-nothing for a session;
no failed file check permits partial selection. A selected session with two names for the same inode is initially
unsupported and retained: partial deletion could otherwise change the metadata
of another remaining file in its own intent. Other sessions may share its inodes.

D4. Retire one session at a time, preserving the existing 32-session attempt limit.
Acquire participating locks in sorted literal-path order, once per distinct
lock file description; reuse that held description for repeated references.
Use nonblocking acquisition. Pass the owned lock context into mutation/replay,
not a reopened descriptor that would contend with itself. Keep all participating
locks held through mutation. Recompute the census under retained locks for the next selection after each
retirement; prior removal changes peer link counts/ctime. Do not use stale census
rows. Discovery, open locks, total census entries and record bytes must have
explicit finite bounds. The initial implementation uses at most 256 session
candidates per attempt, 256 held peer locks, 65,536 file entries per unit, 4,096
files per selected session, 1 MiB per active record and 512 retirement records.
If the complete closure cannot fit those bounds, retain the session and report
that limitation; never sample a closure or raise operator budgets/floors.

## Record version and replay

D5. Preserve version-1 active/terminal records and their existing strict replay.
A new linked session uses version 2 of the cleanup-only
`ci-legacy-incremental-retirement` record, not a new freshness-proof version.
It has the existing original binding, command, physical root/chain/session
identities, worker, relative path, file rows, logical bytes and false shipping
flag, plus a `links` object:

- `sessions`: records sorted by unique literal relative session root in this
  same unit, each with `relative`, `directory` and `lock` identities. A directory
  identity is `[device,inode,mode,uid,gid,mtime_ns,ctime_ns]`; a lock identity is
  `[device,inode,mode,uid,gid,nlink,size,mtime_ns,ctime_ns]`. Peer directories and
  lock objects must match these original values, not just their paths. A lock
  remains a regular empty single-link file with the original owner and safe mode;
- `files`: map from each selected file with link count >1 to sorted unique
  `[session_index, file_basename]` endpoints;
- every map entry has exactly the recorded link count, includes the selected
  session's original endpoint once, and resolves to the recorded inode/metadata;
- unlinked files require no entry; unreferenced session roots are invalid.

The existing common fields remain `version`, `kind`, `state`, `binding`,
`command`, `relative`, `worker`, `root`, `chain`, `session`, `bytes`, and
`shipping_authority`. `binding` uses `validate_binding` unchanged, `command` is a
v4 UUID, and `worker` is `[uid,gid]`. Root/session/chain stable identities are
`[device,inode,mode,uid,gid]`; `chain` has target, profile, incremental and unit
identities. An active record also has sorted unique `files` rows:
`[basename,device,inode,mode,uid,gid,nlink,size,mtime_ns,ctime_ns]`. Version 2 adds
only `links`. Completed records drop `files` for `file_count` and
`inventory_sha256`; version 2 also drops `links` for `peer_count`,
`endpoint_count` and `link_scope_sha256`. Counts obey the original census bounds.
All schemas reject unknown fields and malformed/boolean-as-integer values.
`file_count` is the number of original selected file rows. `peer_count` is the
length of `links.sessions`, including the selected session; `endpoint_count` is
the sum of endpoint-list lengths in `links.files` (linked files only). Hashes use
SHA-256 over UTF-8 `json.dumps(value, sort_keys=True, separators=(',', ':'))`:
`inventory_sha256` hashes the original `files` list, and `link_scope_sha256`
hashes the complete original `links` object. Compact history summarizes that
original intent, not the smaller inventory remaining after an interruption.

The selection API gains a typed internal `allow_closed_links` boolean. Its
strict default preserves version-1 single-link selection. The pre-worker caller
selects the reviewed closed-link policy explicitly. Both policies in the new
controller still reconcile all recognized pending formats before workers start;
the option is not an old-reader emulation or permission to ignore pending state.

D6. Commit root-private intent and fsync its namespace ancestors before any move.
An abrupt process stop before publishing `receipt.json` cannot authorize source
mutation. Reconcile only an empty allocation or one regular, controller-owned,
single-link mode-0600 `.preparing-[a-z0-9_]{8}` atomic-writer temporary file,
bounded by 1 MiB with no xattrs, inside the original mode-0700 controller-private
allocation. Validate the whole envelope before unlinking it and fsync its parent.
Unknown files, links, unsafe metadata, more than one temporary file, or a private
`data` directory without published intent retain the admission hold. The same
validated temporary-file cleanup may precede replay of an existing valid intent.
The original session is moved into the existing controller-private `data` path.
Before mutation, validate the original root/chain, source-versus-data exclusivity,
all remaining file identities and all remaining linked files' recorded endpoints.
Reacquire and hold every required peer lock when replaying a pending record,
and validate its recorded identity against both path and held descriptor.
For the selected endpoint, resolve to the original source before move, or to
`data/file_basename` after move. Before move, the selected session's full
recorded directory identity must match. After its authorized move/partial deletion,
validate its stable first five directory fields; its directory times legitimately
change. Unmoved peer directory and lock identities must match fully. For selected
files still present, every other endpoint remains in its recorded recognized
session and must still have the exact original file metadata. Verify these peers
still have a supported complete session inventory. Endpoints belonging only to
already-removed selected files are no longer replay obligations: unlinking that
selected endpoint legitimately changed the survivors' nlink/ctime. Do not remove
or recreate a peer endpoint while reconciling this history. Changed, missing,
foreign, busy or rebound state holds without deleting more data.

D7. The selected session has at most one endpoint per inode. Therefore removal of
one selected file does not alter nlink/ctime of another remaining selected file.
A partial `rmtree` replay accepts only the unchanged subset of original remaining
files; removed files are not recreated. A completed record replaces full file and
link lists with their hashes and counts. It remains historical cleanup evidence,
never reuse or merge authority. A physical-cache mismatch fences pending replay;
valid completed history remains readable after physical cache replacement.
Old readers must reject/hold version 2 rather than reinterpret it as version 1.
For version 2, if both original source and private data are absent, there is no
remaining file mutation: after validating the original physical root/chain and
intent, commit compact historical completion without requiring obsolete peer
identities/locks. A still-present empty private data directory needs its stable
identity validated before removal. Preserve version-1 strict replay unchanged.

## Callers, validation and rollout

D8. Keep current bounded, unique retirement-ID acknowledgements through the VM,
Python adapter and Rust audit. One ID still represents one session. No duplicate
allocator, parallel journal or new shipping proof is introduced. Original
accounting, reservations, owner holds, image capacity and resource policy remain.

The existing guest result carries `legacy_incremental_diagnostic`: `null` only
for planning that has no generation/cache, otherwise a strict version-1
`ci-legacy-incremental-diagnostic` object. Its fields are `version`, `kind`,
`command` (the original v4 UUID), `cache_identity_sha256` (the original physical
cache binding), `policy` (`strict-single-link` or `closed-links`),
`inspected_sessions`, `retained_sessions`, `retired_sessions`,
`replayed_sessions`, `unrecognized_entries`, `unsafe_namespaces`, `retention`,
`bounds`, and `shipping_authority:false`.
The counters describe observed selection attempts in the recognized cleanup
scope, not a total inventory of unknown cache data or a free-space estimate.

`inspected_sessions` counts new session selections attempted, excluding replay;
`retired_sessions` equals the returned ID count, including completed replay;
`replayed_sessions` counts IDs completed from original pending intent. The
`retention` object has exactly four counters: `unsafe_session`, `unclosed_links`,
`lock_unavailable`, `bounded`. Each unsuccessful attempted selection increments
one counter. Their sum equals `retained_sessions`, and
`inspected_sessions = retained_sessions + retired_sessions - replayed_sessions`.
Selection/retention counters are integers 0–256; retirement/replay counters are
0–32. Booleans are not integers. `bounds` is a sorted unique list drawn from
`session_candidates`, `session_files`, `census_entries`, `record_bytes`,
`attempt_retirements`, `unit_entries`, `unit_directories`, `history_records`. A reached bound can
leave uninspected scope; the report does not estimate its size.
After all existing history has been validated/settled, a full valid 512-record
history retains new selections and reports `history_records`; it does not invent
new history slots or reset completed records. Malformed/over-bound existing
history and unfinished replay remain typed holds with no successful result.

`unrecognized_entries` counts encountered names outside supported unit/session
or session-lock patterns within the bounded lists already read. It is an integer
0–2^22, derived from the two profile lists and at most 1,024 units/profile with
at most 1,025 observed entries/unit. `unsafe_namespaces` counts encountered known
target/profile/unit namespaces whose ownership/type/mode/xattr validation or
enumeration fails, except absent optional profiles; it is an integer 0–4,096.
Neither counter follows or deletes unknown objects. A positive value in either
counter triggers R's separately admitted read-only diagnosis; their values are
observed entry/namespace counts, not distinct inodes, total retained bytes or
proof that uninspected namespaces are safe.

The Python adapter requires and validates this object against the original
generation/command and retirement IDs, then forwards it in its existing result.
The ordinary pre-worker caller explicitly selects `closed-links`; its adapter and
Rust consumer reject a different policy. Rust independently validates the exact
shape, enum/count constraints, command and physical cache binding, then records
the diagnostic through the existing protected controller audit with the original
source binding and verified command-quiescence receipt. ID acknowledgement and
diagnostic are separate fields of one result, not competing cleanup journals.
The preview carries the same diagnostic, allowing its explicit strict reference
policy; both its Python controller and Rust boundary validate the actual selected
policy. A malformed/missing normal diagnostic is a cleanup infrastructure hold.
Error/unknown-teardown paths retain their existing typed hold and durable intent;
they do not emit a successful diagnostic or authorize workers.

D9. Repeat strict-single-link and closed-link policies on the same immutable
production base under one original storage admission, in separate temporary-write
views. Both use the current trusted controller; do not call the strict selection
policy a replay of an older executable. Require:
identical base-image bytes/identity; read-only descriptors before `cont`; identical
protected inventory including compiled artifact contents and regular-file
metadata; report each policy's exact initial/final available bytes, physical
base identity/SHA and retirement count. Require the closed-link policy to recover
more worker-available capacity than the strict reference on that same base.
This establishes the mechanism's capacity benefit, not restoration sufficiency.
There is no existing trusted guest working-set-byte estimator: the current
admission checks host reserves/capacity, not the next workflow's required free
bytes. Do not invent one or substitute an arbitrary threshold. Production
recovery remains blocked until the intended source-correct required local-check
plan completes under the original image/resource budgets without storage failure.
C/R own that sufficiency gate; it becomes executable when the required C producer
and plan are supported. Until then retain the explicit blocking open thread.
Report exact observations; do not infer that the application builds, that third-party seeds
are trusted, or that legacy compiled artifacts can certify current CI.
The protected inventory uses the existing preview helper's exact fields: encoded
literal path, inode, mode, UID/GID and xattrs for each included object; regular
files additionally include link count, size, mtime/ctime nanoseconds and content
SHA-256, while symlinks include their literal target bytes. Access time is excluded
from this comparison (inventory reads still use `O_NOATIME`). Directory times and
link counts are excluded because namespace removal changes them legitimately.
Only `target/debug/incremental`, `target/release/incremental` and
`ci-legacy-incremental-retirements` are excluded subtrees. This byte/metadata
preservation diagnostic cannot certify application input closure or reuse.

D10. Production activation remains gated by the original protected maintenance
rollout, independent review and old-executor census/drain, matched deployed builds,
source/provenance proof and merge enforcement. The cleanup controller alone does
not fence every possible old executable. Pending records must be settled before
any new worker starts; unknown old writers keep admission fenced.
Replay may settle at most 32 sessions per command. If any additional recognized
pending intent remains after that budget, raise the existing cleanup hold rather
than return ordinary success and launch workers. Completed records remain durable
cleanup history even when this command cannot return a successful ID reply. The
existing infrastructure-retry path, after verified command teardown, drains the
remaining work under a new original admission without model repair or accounting
reset. A reply's bounded IDs never stand for uncompleted sessions.

## TDD build order and testable exits

1. Reproduce retaining a session whose object has a known peer inside the same
   unit. Add negatives for a foreign alias, unknown/busy peer, internal duplicate
   inode, malformed/oversized closure, physical replacement and changed peer.
2. Implement bounded locked census and version-2 persistence. Keep version-1
   regressions green. Assert compiled artifacts/fingerprints/downloads unchanged.
3. Fault-inject before intent, during move, partial deletion, after deletion before
   compact completion, and during recovery. Both selected/peer lock failures and
   source/data conflicts retain evidence. Replay must make safe progress after a
   supported interruption, without a manual cache wipe or accounting change.
4. Run native read-only production rehearsal and verify all D9 observations.
   Inspect protected original command/cache bindings and actual free-space gain.
5. Review the exact implementation and remaining source/proof/rollout interfaces
   before production. These exits do not complete epic acceptance on their own.

## Glossary and open threads

An endpoint is a literal directory entry referring to an inode. A closed link
census accounts for all current endpoints of that inode inside the supported
cleanup scope. A cleanup binding describes the original admitted operation; it
is not a passing-CI proof. Logical bytes may count multiple aliases and do not
measure recovered capacity.

R owns implementation and native capacity/preservation validation. C/R own the
load-bearing guest working-set/sufficiency thread: it blocks production recovery
until the intended real source-correct plan runs without capacity failure under
unchanged budgets. A larger synthetic free-space number alone cannot close it. A failure of
that rehearsal blocks production recovery; the trigger for revision is the
measured retained scope or failed invariant. Internal duplicate-inode sessions,
foreign aliases and over-bound closures remain retained until a separately
reviewed extension can handle them; their carrying cost is explicit lost capacity,
not an implicit authorization to prune. Final C API/audit and M/rollout ownership
remain as defined by #1807 and are not discharged by this amendment.

R owns validation and review of the D8 retained-scope reporting carrier (S26/S46).
The local implementation is not yet native-validated or independently reviewed.
That gate blocks production handoff. Low capacity gain, a reached bound or unsafe
unknown namespaces trigger a separately admitted read-only R diagnostic when the
bounded selection counters cannot explain all retained data. Do not infer total
retained scope from logical bytes, selection counters or retirement count.
