# Native declaration readiness (#1704)

This change closes two admission discrepancies in the existing P readiness API.
An API-key shortcut previously accepted `refresh_auth=true` without the imported
controller credential required by native launch. Routed builders could bypass the same refresh-grant check. Credential and runtime
readiness also accepted policies with invalid native memory/session/storage
limits. Both now share read-only declaration checks with launch. Refresh remains
opt-in; inspection neither imports nor refreshes credentials. Existing valid
subscription profiles and read-only defaults are preserved.

`credential-invalid` identifies an unusable credential grant, while
`policy-invalid` identifies an invalid launch declaration. These remain separate
from expiry, missing credentials, missing runtime, reviewer independence,
resource delegation and transient capacity. Existing CLI, builder and reviewer
admission paths consume these core reports. A usable declaration never sets
`live_verified` or establishes a live-ready workflow.

## Test evidence

The packaged CLI RED fixture uses real temporary Git repositories, private
synthetic credentials and a provider executable that records any invocation.
It asserts the production `autopr readiness --json` response, unchanged files,
no provider invocation and no credential disclosure. Against the deployed
parent revision, seven cases incorrectly returned `declared-usable` and an eighth routed case
incorrectly reported its protected builder `declared-available`. A ninth case
misclassified an absent selected policy file as `policy-invalid`; runtime
inspection now distinguishes `policy-missing` from invalid content.
The exact baseline SHA, executable hash, command and exit code are recorded in
[the proof artifact](1704-readiness-declaration-proof.json). Rust behavior tests
also exercise valid imported grants, opaque subscription tokens and invalid
launch limits using a fake clock. No live inference is required by these tests.

Validation on source `11dbcd149828c56df0c6d653c6891fecc6790d4c` completed:

- CLI readiness: 9 tests passed, including the packaged nine-case regression.
- One full core run: 750 library tests passed, 16 ignored, and one unchanged
  process-tree signal fixture failed because this nohup-launched test runner
  inherited SIGHUP ignored (`kill -HUP $$` returned 0 instead of 129).
- Restoring SIGHUP to its ordinary default in the test launcher only made that
  isolated fixture pass. No production signal disposition or source changed.
- Cargo stopped after the library failure, so remaining targets ran separately:
  126 integration tests passed across 16 targets, 24 ignored; doc tests had
  zero failures and one existing ignored example. No second full library run.

The proof artifact retains the original failure, exact commands, launcher
normalization, binary/log hashes and all continuation results. The new CLI
integration target runs the packaged fixture through normal Cargo test discovery.
The six earlier managed admission deferrals were resolved after the host lease
was released; no cache or storage-safety rule was bypassed.

## Production handoff: activation and acceptance remain pending

Read-only inspection on November 15 found both services using the same executable,
selected policy and effective configuration hashes; the artifact records that
observation. Credential metadata was unexpired for both subscription providers.
A read-only installed-parent diagnostic also verified guest/build runtime hashes,
found all three role declarations available and preserved policy/credential files.
Its observer shell lacked delegation and it correctly kept `ready=false`.
These are configuration observations, not current-build guarded execution,
fresh independent review, successful refresh or issue-to-merge acceptance.

Only the orchestrator/Nolan should execute the following after required CI,
independent review and protected merge, from the canonical deployment checkout
`/home/nolan-makatche/AugmentAgent`:

1. Take the supported backup with `augmentagent deploy snapshot --label autoship-1704-readiness`.
   If storage refuses it, report the refusal; preserve
   retention limits and active leases.
2. Retain the current explicit profile and grants. If selection provisioning
   needs replay, run the following from that canonical checkout:

   ```sh
   AUGMENTAGENT_NATIVE_VM_POLICY=/home/nolan-makatche/.local/share/augmentagent/native-vm-autoship/policy.json scripts/ensure-autopr-worker.sh
   ```

   Never run it
   from this lane worktree. This stages both service bindings without restart.
3. Allow the existing normal updater timer, or trigger its existing oneshot with
   `systemctl --user start augmentagent-update.service`. Respect its active-work
   deferrals. Do not restart the worker directly.
4. Use the process-bound deployment inspector and `autopr readiness --json` to
   record exact daemon/worker executable, policy and effective config hashes and
   named role/runtime/resource blockers. An operator shell lacking delegated
   resources is not proof that the worker lacks them. Keep old runtime generations
   until leases drain; no credential grant change is implied.
5. Using the newly packaged code in an isolated guarded fixture, record a native
   file edit and fresh independent review on the declared route. Break that
   fixture's route deliberately and prove refusal before mutation/model spending.
   Preserve the production profile, workflows and owner holds. Record those live
   artifacts separately; an older binary or staged profile cannot satisfy them.

Do not close #1704 from this code change alone. The current-build live probes,
normal updater receipt for this revision and E's end-to-end acceptance remain
separate exits. Health's consumption of effective-worker role readiness belongs
to H; deployment equality alone cannot attest to live provider usability.
