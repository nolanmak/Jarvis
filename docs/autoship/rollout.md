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
4. Add only the native-policy environment entry described in
   [native-vm-launcher.md](native-vm-launcher.md) to a new private drop-in for the
   configured daemon. Preserve all existing drop-ins and `ExecStart`. Stage it
   before the normal updater restart; environment-only staging is not enrollment.

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
