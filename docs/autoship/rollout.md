# Autoship production rollout

This is a procedure, not a deployment receipt. Do not infer completion from
transport-only proofs or fixture tests. Record actual evidence in a private copy
of [the receipt template](rollout-receipt.template.json); unknown fields remain
null. Never put credentials, whole environment dumps, or provider payloads in
that receipt or public issue comments.

## Acceptance before rollout

Record final integration head, hosted CI, and independent review. Require actual
fresh issue → scoped acceptance → guarded builder/tools → committed verifier →
three-role review, seeded defect rejection, and source-less adopted-PR recovery
on the intended packaged boundary. Native Codex final adjudication must resolve
contradictions with exact evidence bindings while mandatory CI remains binding.

Direct Codex/Claude and routed-provider inference/continuation proofs establish
only their recorded transport scope. They do not prove full guarded MCP execution
or issue-to-main behavior. At preparation, full-pipeline proof was still pending;
replace that status only with its terminal artifact, never an in-progress handle.
Real credential rotation followed by a fresh, distinct native conversation also
remains required before claiming durable refresh support. See the
[version-pinned Codex refresh protocol](provider-refresh-protocol.md) for request
and optional-response-field semantics; source evidence is not a live proof. Read-only credentials
must remain unchanged. Enabling `--allow-auth-refresh` is an explicit additional
service-token grant and does not waive rotation/serialization/failed-refresh
recovery acceptance. Do not describe read-only continuation as a refresh proof.

## Preserve state and stage artifacts

1. Read the actual daemon/updater unit identities from the installation and
   updater configuration. Record safe service properties, existing drop-in
   hashes, deployed executable hash and build stamp. For the current default:

   ```sh
   systemctl --user show augmentagent.service --no-pager \
     -p MainPID -p ExecStart -p FragmentPath -p DropInPaths -p ControlGroup \
     -p Delegate -p DelegateSubgroup -p MemoryMax -p TasksMax -p CPUQuotaPerSecUSec
   ```

   Do not dump `Environment` or authentication files. Observe resource settings,
   rather than treating the previous 10 GiB/2048-task installation as permanent.
2. Once the managed storage lease is available, use the installed CLI:

   ```sh
   augmentagent deploy snapshot --label autoship-1598-rollout
   ```

   Record its snapshot ID and manifest hash. Use `deploy pin NAME --reason TEXT`
   if retention needs an explicit rollback dependency. Do not make raw database
   copies or unregistered backup directories. This snapshot covers its declared
   database/binary scope, not arbitrary external provider configuration.
3. Provision a stable owner-private production profile outside every repository
   and worker root. The following placeholders must be replaced by reviewed
   paths; none is an inferred credential or destination:

   ```sh
   /usr/bin/python3 scripts/provision-native-vm-probe.py \
     --directory STABLE_PRIVATE_ROOT --runtime-config REVIEWED_RUNTIME_JSON \
     --provider codex --native-executable EXACT_CODEX_EXECUTABLE \
     --auth-file EXPLICIT_AUTH_FILE --gateway-policy REVIEWED_CODEX_ROUTE_JSON
   /usr/bin/python3 scripts/provision-native-vm-probe.py \
     --extend-policy STABLE_PRIVATE_ROOT/policy.json --provider claude \
     --native-executable EXACT_CLAUDE_EXECUTABLE \
     --auth-file EXPLICIT_AUTH_FILE --gateway-policy REVIEWED_CLAUDE_ROUTE_JSON
   ```

   Include only proven intended providers. Routed profiles require explicit
   `--environment-key NAME` entries and exact reviewed routes. Do not copy a
   temporary probe policy with temporary state paths into production. Retain
   the stable `state_root`, prior immutable runtime generations until runs drain,
   profile/runtime/executable hashes and existing grants. Extension refuses
   conflicting runtime bytes; resolve through a separately validated runtime
   upgrade rather than overwriting active artifacts. Do not copy a host home.
4. Once the supported installer revision is in the canonical deployment checkout,
   explicitly select the stable policy for both services:

   ```sh
   AUGMENTAGENT_NATIVE_VM_POLICY=STABLE_PRIVATE_ROOT/policy.json \
     scripts/ensure-autopr-worker.sh
   ```

   Run this from the canonical deployment checkout, never a task worktree (the
   installer binds the worker to its repository). It writes the private shared
   `autoship-native.env` selection, preserves existing daemon environment files
   and resource limits, and installs both service bindings without restarting
   them. A staged selection is not evidence that running processes use it.

## Protection, normal update, and readback

5. Re-read effective main rules and legacy protection. Apply the reviewed
   [protection proposal](main-protection-rollout.md) only after checking it
   against current settings. Preserve existing ruleset25861241 and any newer
   guards. Never replace an existing policy blindly. Required CI must be strict
   and enforced for the actor; no CodeRabbit or comment-only Claude approval is
   required. Record policy readback, including required app/context identities.
6. After the approved commit reaches main, allow the installed updater timer to
   run, or start its existing oneshot unit (default
   `systemctl --user start augmentagent-update.service`). Its normal
   `scripts/check-for-updates.sh` build, `configure-run-resources.sh --install`,
   guarded restart, executable verification and build-stamp write must complete.
   Respect live-work/resource deferrals; never erase the stamp, override
   `ExecStart`, use `--apply`, or force a daemon restart to bypass them.
7. Run `AUGMENTAGENT_RESOURCE_UNIT=ACTUAL_DAEMON_UNIT
   scripts/configure-run-resources.sh --check` after restart. Read back delegation,
   daemon subgroup, unchanged memory/task ceilings and preserved stricter CPU
   quota. Verify the new PID's `/proc/PID/exe` resolves to the canonical checkout
   release executable, its hash matches the built artifact, and `built-commit`
   matches the intended deployed head. A still-old PID/stamp is a pending update,
   not success. Check only boolean presence/value equality for resource/native
   policy keys in the process environment; do not log unrelated values.
8. Retain actual protected per-run admission/limits, model-dispatch accounting,
   audit health and descendant cleanup receipts on the deployed service. Verify
   unavailable capacity stays unpaid before dispatch and resumed workflows keep
   their original bindings. Unit readback alone does not prove a native run.

Record daily issue-to-main consistency, backlog progress and scheduled-email
restart/retry observations separately. These are longer-window acceptance, not
implied by one successful rollout or transport call. If rollback is required,
use owned deployment artifacts and the normal guarded deployment path; retain
pending workflow/runtime evidence and do not introduce an executable override.


## Red-main repair diagnostics

A red-main repair names failed required CI, without assuming that missing Rust
test names mean compilation failed. Its bounded excerpt uses GitHub's failed
steps for each verified job, rather than successful setup/tests or post-job
cleanup. Check names, job identities and links stay with their diagnostic text.
Missing logs remain explicitly unavailable; they never change a failed verdict.
The public excerpt is limited to 2,500 bytes, with shared space for up to eight
failed jobs and an explicit omitted-job count beyond that. Oversized excerpts
retain both ends and mark truncation; inspect the job link for full evidence.

Changing diagnostic wording does not replace an existing marked repair issue
for the same full main SHA. Existing requests, owner holds and accounting remain
attached to that issue. Lines rejected by the public-report privacy check are
replaced with an explicit withholding marker. Full-report validation still gates
issue publication; withholding diagnostics never turns a failed check green.

## Review-held triage

Read the workflow's current state before requesting another attempt:

```sh
augmentagent autopr status --workflow WORKFLOW --json
```

| Recorded reason | Meaning and recovery |
| --- | --- |
| `no-reviewer-capacity` | Reviewer selection found no eligible serving provider. Inspect the declared providers and cooldowns. |
| `review-evidence-budget` | Typed evidence collection or request limits were exceeded. Decompose the evidence within the existing bounds. |
| `review-recovery-required` | A selected review failed to complete. Inspect the protected controller error and repair its recorded cause before retrying. |
| `review-main-advanced` | Main advanced outside the captured candidate. The original workflow queues integration and fresh verification with a five-minute backoff. Automatic selection recognizes this dependency wake within the same day, and the short queue poll notices eligible work without requiring a CLI job. Existing owner holds and unresolved operations still prevent dispatch. |

Private progress retains the typed review blocker when a worker wrapper finishes.
A resume acknowledgement is still intent, and can wait for native retirement or
another admission guard. If a format correction remains incomplete, inspect the
protected role/group/validation diagnostic before another paid attempt. The
controller can reuse already-captured candidate and pinned source in that same
bounded correction; the two-observation limit remains unchanged. Diagnostics do
not establish failed-result replay or independent approval.

A review hold does not establish whether model work was billed. Inspect the
workflow's resource counters and confirmed-merge allowance separately. Historical
hold reasons remain historical records; a new deployment does not rewrite them.

Hosted APT setup requests IPv4 on hosted runners and retries a confirmed Ubuntu
mirror DNS failure once against
`https://mirrors.edge.kernel.org/ubuntu/`, using the runner's Ubuntu suite and
archive signature keyring. Temporary source options apply only to that retry;
host DNS and source files stay unchanged. Signature, lock, package and subsequent
fallback failures remain failures. The installer is part of the controller's
pinned CI policy, so changing it requires the normal controller rollout.

After repairing an internal execution failure, authenticated `autopr resume
--workflow WORKFLOW` can queue recovery of the preserved attempt. Use the
installation's native/resource policy selection; an unconfigured shell can
report `policy-missing` while the services have a valid selection. An accepted
resume can still require runtime retirement reconciliation before execution.
Acceptance, cleanup, model progress, and a confirmed merge are separate results.
Preserve unrelated owner pauses and inspect the resulting status.

Both fresh-issue selectors consult that same owner control using the canonical
repository/issue identity before triage. They skip paused issues and consider
eligible peers without changing pauses, candidates, history or resource charges.
An all-paused inventory is idle; a supported owner resume restores selection
eligibility. This free read does not admit execution: on the primary route, the productive
claim still checks owner controls atomically, including a pause arriving after
selection. The legacy configured multi-repository route currently has only the
selection filter; #2056 owns its missing durable claim/model-admission integration.
Primary-route claim tests do not prove that configured execution is fenced.

Primary automatic selection also skips authors who fail the existing owner,
write-association or explicit-author allowlist policy. It continues through the
complete inventory to authorized peers without commenting on or labeling those
untrusted issues. They still need explicit owner authorization; neither a bot
identity nor a label grants it. The final execution trust guard remains in place,
and the configured legacy approval policy is unchanged.

A fresh-selection `owner-control-unavailable` hold means the existing protected
control state or canonical identity could not be read reliably. It is unpaid;
inspect the configured state and its access rather than treating unknown state
as permission or clearing owner controls. A missing database for a new
installation is different: free selection may consider new issues without
initializing a database or registering workflows.

## Capacity waits in health readback

After verifying the deployed build and worker, run:

```sh
augmentagent autopr-health --json
```

`resource-capacity-waiting` or `native-capacity-unobserved` is an ALERT for active durable
`resource_capacity` / `await_capacity` checkpoints. A running worker, recent
manual merge or provider-hold warning does not clear it. Owner-paused, held,
terminal and diagnostic-only workflows are excluded. The count describes the
persisted wait; available capacity still needs normal worker reconciliation.

`capacity-state-unavailable` is an ALERT when the running worker's effective
configuration or selected workflow store cannot be verified. Health reads the
worker's private process-bound post-bootstrap receipt and opens that workflow
store read-only, using the same selected database and daily cap for capacity and
merge accounting. Missing or stale evidence does not select or create another
workflow database. Capacity ALERTs make the command exit 1. Native retained-state measurements are descriptive storage observations, not host-memory sizing or execution authority.

Health observation does not claim, wake or prune workflows or spend shipping
allowance. Inspect the worker's selected capacity and ownership evidence through
its configured delegated resource context. A readiness probe from an ordinary
shell may lack delegation without proving the worker lacks it. Restore supported
capacity or complete verified ownership recovery; preserve owner holds, retained
sessions and budgets while the underlying cause is unresolved.


## Observed deployment, 2026-10-26

The normal updater deployed `c17ef74eefea6be6324778fad3522150c356902a`
at 20:32:01 UTC after the active workspace gate ended. Its log records daemon
PID 2377891 replaced by 3004552 and the build stamp written. Readback confirmed
`Delegate=yes`, `DelegateSubgroup=daemon`, CPU quota 4s, MemoryMax 10737418240,
and TasksMax 2048. The running executable resolved to the canonical release
artifact; both SHA256 values were
`ad7831c2a25d1e8aaacdc8a927b98aea7030456691ea005ca3f2f9ff5fad92e6`.

This establishes deployment of PR #1603 and resource-unit configuration only.
The stable native policy remained staged, not selected by the daemon, and
shared local/GitHub CI PR #1616 was still under validation. Protected native
issue-to-PR acceptance and sustained autonomous shipping remain outstanding.
