# Host memory facts and default sizing component

Part of #1918/#1919 under #1917/#1703. This is a separable implementation
contract for the existing `scripts/run-resource-scope.py`, not complete C0,
automatic activation, selected-worker observation or admission permission.
The full [working contract](host-capacity-contract.md) remains authoritative.

## Scope and dependency boundary

Implement and test the settled M1-M3 memory units/equations as data helpers,
and strengthen the existing supervisor's `available_memory()` input decoder.
The current path trusts the second token on the first MemAvailable line without
validating its unit, duplicate values, total memory or u64 range. Bad input can
therefore become an apparently finite capacity measurement.

These helpers perform no writes, cgroup operations, cleanup, admission, provider
dispatch or configuration changes. They cannot return an overall available
capacity status or a launch/release token. The existing run demand and admission
headroom policy remain their current values until supported full activation.
The planner is not connected to installation/admission in this component.

Implementation of this closed component may proceed while the full C0 storage,
publication and migration contract remains open. C1 completion, C2-C5 and live
activation still require their complete reviewed prerequisites. This explicitly
replaces the blanket prohibition on all C1 helper implementation; it does not
make partial resource discovery a complete detector or close the epic.

## Exact functions

`parse_host_memory(data: bytes) -> dict` accepts at most 65536 ASCII bytes and
returns exactly `total_bytes` and `available_bytes` as Python integers within
u64. Require exactly one MemTotal and one MemAvailable field; each has a decimal
ASCII unsigned count and exact `kB` unit, with no extra tokens. Linux meminfo kB
means 1024 bytes. Ignore other fields, never use swap as physical memory. Total
must be positive; available may be zero and cannot exceed total. Missing,
duplicate, negative, signed-plus, nondecimal, non-ASCII, wrong-unit, oversized or
overflowing required data raises RuntimeError with a fixed safe diagnostic.
Unknown is an exception, never a numeric zero/default/unlimited result.

`read_host_memory() -> dict` reads at most 65537 bytes from `/proc/meminfo` in
binary mode, invokes that parser and returns its data. OSError remains an
observation failure. `available_memory()` delegates and returns available_bytes,
so real admission uses the strengthened parser. This is a single host-context
read, without worker/cgroup/freshness qualification; O1-O8 must still supply the
complete selected-worker observation before full automatic admission.

`memory_defaults(total_bytes, external_maxima=(), reserve_bytes=None) -> dict`
returns exactly effective_bytes E, protected_bytes H, memory_max_bytes B and
memory_high_bytes floor(9B/10). Inputs are true Python integers (not bool),
within u64. Total is positive; each external maximum is either a finite value
including zero or None for an explicitly observed kernel `max`. Missing/failed
ceiling reads must raise before this function, never be converted to None.
Only list/tuple external vectors are accepted. Their values are already selected
by the actual observer; this function neither identifies/excludes managed scopes
nor claims that the vector is complete or fresh.

E is the minimum of total and all finite external maxima. H is
max(1 GiB, ceil(E/4)); a provided reserve must be at least this default and raises
H. A reserve below the protected default is rejected. B=max(0,E-H). A valid
reserve greater than E produces B=0, not a fabricated fit. Compute exactly
without float rounding, saturation or u64 intermediate overflow. Return only
bounded integers. Invalid arguments raise RuntimeError with fixed diagnostics.

No generic raw-ceiling string parser, CPU/PID/FD/storage planning, managed-scope
provenance, resource reservations, persisted cache, policy writer or installer
hook is added here. These are remaining work with their existing owners/gates.

## Behavior-first validation

- Existing available_memory entry: valid units, zero availability, missing total,
  duplicate available values, wrong unit, invalid counts and overflow.
  Record behavioral failures against unchanged main separately from new-API
  absence; a missing helper is not evidence of a deployed sizing bug.
- Exact 2/8/16/64 GiB and 64 GiB/2 GiB ancestor fixtures from M3, unlimited
  leaf/finite ancestor, finite zero, explicit reserve and u64 boundaries.
- Odd-byte envelope rounds H up and MemoryHigh down; largest u64 inputs stay
  exact. Booleans, negative/oversized numbers and an undersized reserve reject.
- Full existing resource test suite; tests requiring an isolated delegated
  service must be reported separately from unprivileged tests/skips.
- A read-only real `/proc/meminfo` smoke sample proves only this host decoder.
  Hosted CI and independent review are required before normal rollout. No new
  local full gate, VM, paid model call or production-state mutation is needed.
