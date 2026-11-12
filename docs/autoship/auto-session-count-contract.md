# Automatic retained-session count semantics

Part of #1918 / #1917 / #1703. This closes one proposed sizing decision;
it does not close C0, authorize downstream implementation, enable auto mode,
or change the deployed native policy. The remaining full-vector, custody,
configuration, publication and rollback contracts remain prerequisites.

## Problem and source boundary

The current provisioner writes `max_sessions=16` and `max_state_bytes=8 GiB`
when arguments are absent (`scripts/provision-native-vm-probe.py`). Native
admission and observation enforce count and bytes separately. The working C0
contract derives a shared per-filesystem retained-byte envelope but leaves the
automatic count behavior unspecified. A larger byte envelope alone can leave
the implicit count ceiling blocking a larger host.

This amendment applies to the current uniform retained reservation S=1 GiB per
session. Retained disk reservations are separate from live memory, task and
CPU reservations. A future variable-charge profile must explicitly amend this
contract; do not infer a session count from memory or physical file sizes.

## Decisions and reasons

D1. Auto mode has no implicit per-root session-count ceiling. An absent count
argument is absent, including when a legacy default would have been 16.
A known explicit per-root ceiling remains an additional admission predicate.
This avoids duplicating the shared retained-byte authority as another default
pool. Fixed mode retains its existing count semantics.

D2. The shared authority derives aggregate count readback from its current
retained-byte envelope B: `aggregate_slots = floor(B / S)`. Existing retained
rows across every registered root on that filesystem consume those slots.
`aggregate_remaining = max(0, aggregate_slots - retained_rows)` is observation,
not a reservation or execution grant. Do not add existing retained bytes to B,
subtract them twice, reserve all remaining slots, or copy this envelope into a
separate maximum for each root. The full per-filesystem demand vector and
original shared admission transaction still decide every create or resume.

B is the total retained-charge envelope across registered roots on this
filesystem, computed for the proposed operation after accounting for its
complete demand. It is not raw free space or an unqualified cached policy value. The existing working S4 rule
is `floor(max(0, F-H-P-D_other-G) / S) * S`: F is current usable physical free
space, H protected headroom, P other shared claims, D_other unresolved active
growth excluding retained rows, and G this operation's new active-growth peak.
Every operand and producer profile must be qualified. These arithmetic rules
neither establish a physical quota nor discount retained commitments.

D3. A create also checks a known explicit root ceiling C, when present:
`root_retained + 1 <= C`. Without that explicit override, no additional root
count guard is invented. A resume consumes no new retained slot; it still
requires its original binding, applicable explicit policy and complete new
active-growth/resource admission. Existing retirement authority alone releases
retained charges; a missing process, small files or age does not release them.

D4. Unknown observations remain unknown and block new auto admission. A known
B<S yields zero available slots while remaining configured auto. If the system
shrinks below current commitments, retain every row and report the deficit;
never delete state or change a count to make the data fit. Work blocked by an
explicit count override remains distinguishable from byte or memory pressure.
Readback must identify a count as derived aggregate observation or explicit
root override; do not label the derived observation as a configured root limit.

D5. Migration preserves known explicit overrides even when their value is 16.
For ambiguous legacy fields, the authenticated enable-auto operation must bind
the exact predecessor and explicitly adopt the named count property before
removing its legacy ceiling. Numeric equality never proves provenance. The
final versioned configuration/native policy schema must represent absence and
explicit override distinctly; no `0`, `usize::MAX` or other numeric sentinel
may impersonate absence in the legacy positive-count policy. That schema and
its startup compatibility fence remain open C0 dependencies.

D6. All byte/count arithmetic is checked, uses bytes with S=1073741824, and
performs division before representability checks. Overflow, unsupported profile
or a count not representable by the selected consumer is unavailable evidence,
not a wrapped, clamped or unlimited ceiling. A valid explicit count of zero is
not introduced by this amendment; existing positive override rules remain.

## Worked fixtures and required behavior tests

These are declared-demand design fixtures, not claims that a production writer
already proves G. Each positive case also requires all omitted resource,
identity, custody, authorization and quota predicates to pass.

| Case | Inputs | Required result |
| --- | --- | --- |
| Large host, absent override | F=79 GiB, H=15 GiB, P=10 GiB, D_other=0, G=2 GiB; 32 retained sessions | B=52 GiB, aggregate_slots=52, remaining=20; no implicit 16-session blocker; a create fits the stated byte/count predicates |
| Shared roots, no copied pool | Same envelope; root A retains 4, root B retains 48 | Aggregate rows=52, remaining=0; A cannot create merely because its own count is small |
| Explicit root ceiling | Same envelope; A retains 4, C=4 | New create fails the explicit count predicate despite aggregate space; no override is rewritten |
| Explicit ceiling with room | Same envelope; A retains 4, C=6; global rows=32 | One create passes count predicates, reserves only that session in the existing transaction, and leaves five A rows / 33 global rows |
| Sub-slot envelope | Qualified unrounded headroom S-1 bytes, zero rows | Rounded B=0, zero slots; configured auto, blocked create; no fake positive legacy policy |
| Exact boundary | Unrounded headroom S, zero rows | B=S, one slot; +1 byte still yields one slot |
| Pressure after admission | B=2S, three retained rows | Preserve all three charges; report one-slot/S deficit and block a create; do not declare an existing resume admitted from this count readback |
| Explicit legacy 16 | Proven caller override C=16 | Preserve C=16 in auto; do not classify it as a default |
| Ambiguous legacy 16 | No proven caller/default provenance | Keep fixed unless a supported hash-bound adoption explicitly names the count property |
| Restart / concurrent create | Two callers see one remaining slot | Readback grants neither caller authority; the existing locked admission can commit at most one full additional retained reservation |
| Unknown / overflow | Missing operand, invalid count or failed checked conversion | Unavailable evidence; no new admission, mutation or unlimited fallback |

These vectors are part of the implementation handoff once the retained parent
contracts are complete and implementation is authorized. That implementation
must first reproduce the old implicit-ceiling behavior as RED, then exercise
the real shared-admission and configuration migration entry points, including
multi-root and restart cases. Those integration tests must pass before merge
or activation. Mathematical fixtures or observer-only tests do not prove the
implementation exits. No integration coding is authorized by this amendment.

## Remaining scope and integration

R owns integration with the one shared storage/admission authority and exact
root/row identity. P owns final schemas, provenance, renderer, supported migration
and effective loaded receipts. H/N owns precise configured-versus-derived private
readback and existing outbox delivery. These owners are work areas under #1918,
not new independent services or allocators.

Full C0 still requires qualified native/control-plane growth and bootstrap,
original proof/handoff APIs, registry identity/replay, all-writer compatibility,
mode/publication schemas, security-preserving reconciliation, snapshot coverage
and loaded/delivery receipts. Those existing gaps continue to block activation.
This amendment introduces no new runtime ledger, cleanup capability, live
configuration change or autonomous merge proof.

The previous working decisions remain historical design evidence at
`799ba04dcfcd51b65f9585f82aa995a2c0bd4dfa` under
`docs/autoship/host-capacity-contract.md` and
`docs/autoship/host-capacity-sizing-proposal.md`. This amendment clarifies their
absent-count behavior; it does not revive the retired integration/docs PR #1916.
