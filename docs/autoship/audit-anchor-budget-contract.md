# Audit anchor writer/reader budget component

Scoped producer-bound repair #1940 under #1918/#1917/#1703. The existing audit reader
accepts anchors up to LIMIT=67108864 encoded bytes. AnchoredAudit::save currently
serializes without a byte bound, including pending log lines and recovery-seal
validation. A line that fits the log cap can expand again when embedded in JSON
pending; a recovery callback can also create an anchor larger than its own reader
accepts. This checkpoint authorizes no change until scoped review is complete.

A1. Keep the existing 67108864-byte anchor and log limits. Every save of an anchor
must enforce its complete encoded JSON size before replacing the anchor target.
Use a bounded streaming Write adapter around the original temporary, enforcing
the aggregate count across all serde write calls. An individual oversized write
must refuse before writing that chunk. Partial underlying writes count only
actual accepted bytes; write errors propagate. No temporary can contain more
than LIMIT bytes. Do not serialize an unbounded Vec first or raise the reader cap.
The existing private same-directory temporary, sync, replacement and parent sync
sequence remains unchanged after successful serialization.

A2. An oversized pending serialization refuses the new append before new log-line
bytes are written or the original anchor replaced. Existing pending recovery may
legitimately finish first under the existing writer lock; no rollback of that
previously committed intent is allowed. On an initially settled chain, both log
and anchor remain byte-identical after the refused new append. The still-open
chain can accept a smaller subsequent record. Do not truncate payload or metadata
and report success. A budget failure is invalid/unsupported data, not physical
disk exhaustion, a sealed generation, a successful append or native release.

A3. For an unsealed generation, validation whose resulting encoded anchor
exceeds LIMIT refuses before replacing the anchor, preserving existing log/anchor
bytes on the ordinary settled fixture. Already-sealed retries retain the original
qualified callback and return the existing proof without serializing a new seal;
an unused new callback result is not a reason to change that existing behavior.
It must not leave a false sealed marker or make the old anchor unreadable.
A later qualified smaller seal still works. Its callback's original qualification
and current authority requirements remain unchanged; this repair creates no
quiescence, owner, cleanup, resource, input, model or workflow permission.

A4. Already oversized/corrupt old anchors remain refused by their existing reader.
No larger cap, metadata stripping, reset, deletion, raw backup or recreated chain
is a recovery mechanism in this repair. Existing exactly-once pending-tail replay,
corruption detection, cross-process writer serialization, original locking and
sealed-generation behavior remain unchanged for supported records.

A5. Exact LIMIT encoded bytes may publish; LIMIT+1 must refuse without replacing
the old anchor. Count JSON syntax, escaping, pending data and seal metadata, not
only payload characters. Serialization errors and underlying write/sync/replace
errors continue to propagate. Only normal original temporary ownership cleanup
is claimed; lost rename acknowledgments, namespace replacement, power loss or
cleanup failure are not reclassified as proof of no mutation.

A6. This is an encoded-file producer bound, not a full process-memory or physical
filesystem profile. The supplied Value/string may already exist in memory;
callback allocation and serde memory are separate demand. The previous anchor,
new bounded temporary, log, lock, metadata and uncertain remnants must all be
included by the full C0 growth/admission producer. No native retained charge or
safety floor is reduced. No deletion/retention/sweeper is added.

## Behavior-first validation and handoff

- Reproduce through actual AnchoredAudit::append a log-sized record whose escaped
  pending anchor exceeds LIMIT, and through actual seal_for_recovery an oversized
  validation result. Record failure against unchanged main before implementation.
- Assert original settled log/anchor preservation, no ordinary owned temporary
  leak, later small append/seal usability, exact encoded LIMIT and LIMIT+1.
- Exercise short writes, zero-progress/failure and cumulative-chunk accounting
  at the serializer boundary without substituting those for real sink fixtures.
- Run the existing audit-chain and protected-audit consumers with
  scripts/cargo-managed.sh; record exact SHA, failures, skips and hosted checks.
- Fresh-reader and complete narrow seam review precede an explicit #1918 exception.
  Independent code approval/hosted CI and normal snapshot/updater rollout remain
  required. No real credential, VM, provider or production state is exercised.

Whole C0/C2/C3, all-writer custody, productive owner ingress and #1703 live
CLI/Discord/autonomous merge/fault/soak acceptance remain separate unfinished work.

## Review and baseline record

Tracked as #1940, a native child of #1918. Fresh-reader review and all 19 mapped
seams were checked in narrow groups. One seal-retry wording ambiguity was
resolved/rechecked: only a newly constructed unsealed anchor is serialized;
already-sealed retries retain their existing qualified callback/proof behavior.
No remaining scoped finding was retained. This is design review, not code approval.

Against unchanged main 416f392b, managed audit-chain tests pass five existing cases
and fail both new real append/seal refusal cases: the writer accepts the oversized
pending/sealed anchor. The append fixture's log JSON is explicitly checked below
LIMIT, distinguishing a real nested-encoding failure from an overlarge log line.
No model/VM/production state is involved. Full C0 and deployment gates remain open.
