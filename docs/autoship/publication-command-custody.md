# Publication command custody (#2164)

The first slice exports `publication_command_custody::PublicationCommandOwner`.
It changes no dispatch, audit, lease, or timeout behavior. The runtime integration
below remains required; the type alone does not fix #2164 or complete #1706.

The production-controller regression at `f1615bab` fails with “active publication
observer ignored its persisted inactivity deadline”. It uses real temporary Git
and protected workflow state, a production SweepGuard, and an injected persisted
deadline. Earlier admission and fixture failures are excluded from that RED.

## Identity and authority

D1. The wire form is exactly schema version 1, role `publication`, nonempty
`workflow_id`, and positive `lease_generation`. Missing/extra fields, unsupported
versions, other roles, and invalid identities are rejected on deserialization.
These are workflow/lease identity data, not authorization or teardown evidence.
Upstream services still establish the canonical workflow and live fenced lease.

D2. `supervision_identity()` supplies the same workflow/generation pair used by
the existing supervisor proof. It does not install a native VM owner, admit a
model call, claim a lease, or change resource or merge accounting. Native VM
ownership and publication command custody remain separate facts. In runtime
integration, a publication-only audit must reject model admission before its
accounting callback and reject native-owner installation. A native-owned audit
cannot be converted into a publication-only audit. Integration must extend the
supervisor registration/cancellation/proof owner lookup to recognize publication
custody while leaving `native_runtime_owner()` absent. The current native-only
cancellation check cannot be reused unchanged for free publication.

S/L1 owns this contract and its timer/cleanup integration. R continues to own
capacity evidence and kernel resource admission. Reuse those APIs; coordinate
any required change to their policy with R. This is not an unassigned dependency.

## Runtime integration contract (not yet implemented)

D3. A free sweep gets a protected audit in a publication-specific namespace for
its exact lease generation. It never reopens the predecessor native audit as its
own command registry and never moves the native audit-generation cursor. An
owned native pipeline retains its original audit and supervisor registry.

S commits a versioned `publication_command_custody_registered` event in the
existing protected workflow journal before a command can spawn. Payload:
`schema_version: 1`, `owner: PublicationCommandOwner`, `audit_path: string`.
The writer validates the current lease/binding and the protected absolute audit
path, rejects inspection-only leases and conflicting registration, and treats
an identical registration as idempotent. No new authority table is needed.

D4. Register each command in the original audit's supervisor inventory before
spawn and check the live lease/cancellation fence again before supervisor release.
Use the existing supervised command boundary, never the model/provider launch
boundary. Use R's existing text-command resource declaration and kernel admission;
do not reserve a builder VM or copy storage lease descriptors to the command.
An identity DTO or a deserialized audit row is never a dispatch grant.

Publication create/ready/push/merge commands and their bound validation calls
must use this boundary. Native publication uses its existing original logger;
free merge observation uses the new publication-only logger. Unrelated inventory
without an active workflow keeps its existing bounded command watchdog.

D5. Consume the existing persisted activity deadline. Commit the fenced idle
request before signalling only that owner's registered supervisors. Cancellation
fences later dispatch. Heartbeats and duplicate output do not extend the clock;
waiting/paused states have observation eligibility instead. The existing
fifteen-minute per-command watchdog remains independent of the inactivity timer.

D6. Live completion uses the original logger's opaque Rust quiescence capability,
covering descendants and drained audit writers. Live completion does not revoke
the audit: successor recovery seals the predecessor audit against further writes. Match its workflow, generation and audit
path to the protected registration. Persist
`publication_command_custody_completed` with schema version 1, original owner,
and original audit path before releasing publication custody. Repeating it has
no effect. Normal completion also requires this proof; absence of an idle request
is not proof that publication commands have stopped.

For interrupted ownership, a current successor lease brackets inspection of the
original audit and kernel scopes. Accept only its predecessor proof issued for
that exact successor, not a newly empty registry. Read outstanding registrations
from the protected journal even if the controller died before recording an idle
request. Resolve predecessor command custody before installing new command
custody. Complete an existing matching idle request through the ordinary fenced
recovery API; do not invent or clear another owner's request.

Unknown cleanup preserves custody, evidence, spend and independent holds. Its
reason is `publication_command_custody_unverified`, action
`await_publication_command_teardown`, unless an independent owner/terminal/input
hold must remain visible. S reconciles it without model calls or builder admission at five-minute observation
eligibility, including after restart. Only verified original teardown/writer proof
clears the blocker. A free observer may inspect a predecessor's native recovery;
its own command cleanup must never claim to settle that predecessor's native
custody. Existing terminal and owner-pause controls remain authoritative.

D7. Local cancellation does not prove a GitHub mutation did not happen. Preserve
publication/merge intents and reconcile the actual remote result through the
existing services before repeating a mutation. Retry debt remains the existing
5/10/20/40/60-minute policy. Neither custody nor timeout consumes merge allowance.

## Delivery and validation

The contract-only slice tests exact wire interoperability and refusal of malformed
or authority-bearing data. It has no runtime permission setter or timer wiring.
It stacks on #2162. The local RED branch remains available for the integration
slice; merge it normally into the chain when implementing that slice.

Runtime exits additionally require the existing production SweepGuard regression
to pass, a real supervised hung command and detached descendant to terminate
before lease release, another owner's command to survive, late dispatch/model/VM
admission to fail, interrupted recovery with and without an idle request, unknown
custody retained for unpaid observation, owner/wait/terminal preservation, and
remote lost-response reconciliation without duplicate mutation.

Use the registered managed cache, targeted tests, then one full relevant suite
with `CARGO_BUILD_JOBS=4` and `--test-threads=4`. For the contract-only slice use
`test -p augmentagent-channel-core --features test-isolation`; runtime CLI wiring
also requires the full CLI suite from #2164. Required hosted checks and
independent exact-head engineering review precede the serial train's merge.
No benchmark, production state edit, repair-PR autoship test, allowance reset,
legacy custody migration, other-role expansion or autonomous-shipping acceptance
is included. Any required legacy migration needs a separate explicit contract.
