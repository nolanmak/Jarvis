# Required-workflow native VM transport

Required autoship loggers select the common VM launcher for Claude, Codex and the
Codex-backed Qwen/GLM adapters. Missing policy, missing provider profile or an
incomplete saved native runtime fails before native dispatch. Interactive calls
without the required-workflow marker retain their existing transport.

Select an existing private profile for both Linux services through the managed
installer (run from the deployed checkout during normal rollout):

```sh
AUGMENTAGENT_NATIVE_VM_POLICY=/absolute/controller/policy.json \
  bash scripts/ensure-autopr-worker.sh
```

The installer persists only the selected path in
`$XDG_CONFIG_HOME/augmentagent/autoship-native.env` (default
`~/.config/augmentagent/autoship-native.env`), mode 0600. Both the daemon drop-in
and independent worker load this shared environment file. Later installer runs
without a selection preserve it. Staging a policy alone does not select it.
The installer neither edits the profile/credentials nor changes refresh grants,
and does not restart services. The normal updater owns activation. Removing the
selection file deselects this shared override on the next normal rollout;
separately configured environment values still apply.

Successful installation proves configuration persistence, not provider readiness.
Before accepting rollout, verify both running processes' selected policy hashes,
packaged executable hashes, runtime integrity, role capabilities, usable auth,
and a current guarded edit plus independent review. Never dump credential values
or complete process environments for that check. Existing runtime generations
remain available while their leases drain.

The daemon reads `AUGMENTAGENT_NATIVE_VM_POLICY` itself, never from model options.
The file must be owner-private, regular and single-link. Its version 1 shape is:

```json
{
  "version": 1,
  "runtime_config": "/controller/build-vm/runtime.json",
  "guest_runtime": "/controller/private-native-vm-runtime",
  "state_root": "/controller/private-native-vm",
  "memory_mb": 1024,
  "max_state_bytes": 8589934592,
  "max_sessions": 16,
  "providers": {
    "codex": {
      "auth_file": "/controller/provider/auth.json",
      "auth_relative": ".codex/auth.json",
      "refresh_auth": false,
      "environment": ["CODEX_API_KEY"],
      "gateway": {
        "version": 1,
        "authorities": [
          {
            "authority": "api.example.com:443",
            "tls": true,
            "allow_loopback": false,
            "routes": [{"path_prefix": "/v1/", "methods": ["POST"], "websocket": false}]
          }
        ],
        "limits": {
          "max_connections": 8,
          "max_request_bytes": 8388608,
          "max_response_bytes": 67108864,
          "max_frame_bytes": 65536,
          "idle_timeout_seconds": 300
        }
      }
    }
  }
}
```

This is a schema example, not a working provider policy. Each enabled provider
needs explicitly reviewed authentication and inference routes matching its
configured endpoint. Unlisted hosts/routes/methods and direct guest networking
are denied. No production policy is installed by this change.

The guest exports no NIC and receives only read-only manifest-pinned distribution runtime, one native
executable, declared prompt/image inputs and the selected provider credential.
Native code runs as a nonroot guest user. A root guest supervisor owns the
virtio control channel and local gateway. The guest MCP relay exposes only the
original host-side policy bridge; host source, controller databases, approval
records and original provider home directories are not mounted.

The existing resource supervisor first starts the trusted VM controller and
waits at most 60 seconds for guest readiness. Only then does normal protected
model accounting admit the call and release the native CLI. Cancellation before
that release reaps the VM and broker without charging a model call. Productive
native execution has no new whole-session wall timeout. A durable dispatch
intent precedes guest release; interruption after it must not imply no call.

Conversation aliases use observed native session IDs. Private runtime exports
are bounded to 64 MiB, reject links/nonregular files and commit an immutable
generation before atomically replacing the completion pointer. Resume refuses
an unfinished initial export and uses only completed generations. Guest OAuth
refreshes persist within that conversation unless controller credentials have
changed independently. The optional `--allow-auth-refresh` provisioning grant explicitly imports the
credential into `state_root/credentials/PROVIDER.json`. Only this controlled
credential may be refreshed. The original interactive authentication source is
never overwritten. Codex/Claude token and timestamp fields may rotate; account
identity, scopes, static API keys and configuration must remain unchanged.
A private writer lock plus generation digest serializes VM refreshes; a visible
independent reimport wins. An uncooperative writer ignoring that lock is outside
this controlled writer protocol. External credential reimport must acquire the
same `.native-vm-refresh.lock` and invalidate stale conversation seeds. Real
fresh-session authentication after rotation remains a rollout acceptance test.

Local executable validation:

- `python3 scripts/test-native-provider-vm.py`: eight tests cover pre-admission
  refusal, cancellation cleanup, completed-generation recovery and symlink
  rejection and undeclared runtime siblings. These do not execute a VM or contact a provider.
- `scripts/native-provider-vm-local-probe.py --execute --runtime-config PATH`:
  actual 512 MiB guest executes a synthetic Python CLI twice, proving nonroot
  execution, root control-file denial, loopback-only networking and persisted
  runtime continuation. Run under a bounded transient unit and coordinate the
  central build/VM queue.

The first development probe passed its initial launch but exposed a `Path.join`
resume typo; the corrected complete probe passed both launches. This synthetic
proof does not establish provider authentication, SSE/WebSocket compatibility,
MCP end-to-end behavior or safe OAuth refresh across distinct conversations.
Those remain explicit rollout acceptance checks before enabling the policy.

The authenticated transport probe is separate from full Autoship acceptance:
`scripts/native-provider-codex-auth-probe.py --execute --policy PRIVATE_POLICY
--executable EXACT_CODEX_EXECUTABLE --resume --output NEW_PRIVATE_DIRECTORY`.
Run it in a bounded transient unit with at least 2 GiB for a 1 GiB guest.
It checks an exact reply, actual native session continuation, completed runtime
export and unchanged source authentication. Private logs retain failures;
public output contains only status and evidence locations. It does not prove
MCP execution, issue-to-LGTM behavior or cross-conversation token refresh.
The full pipeline launcher additionally requires `--vm-policy PRIVATE_POLICY`
and passes that policy into its transient systemd service explicitly.

Provision a disposable profile with `/usr/bin/python3
scripts/provision-native-vm-probe.py --directory NEW_PRIVATE_DIRECTORY
--runtime-config EXISTING_RUNTIME_CONFIG --provider codex
--native-executable EXACT_NATIVE_EXECUTABLE --auth-file EXPLICIT_AUTH_FILE
--gateway-policy REVIEWED_ROUTE_POLICY`. Arguments are paths and declared grants,
not credential contents. The helper never edits daemon configuration. It copies
only the distribution Python packages and resolved ELF dependencies, excludes
site/user customization, and records each file digest. The guest does not mount
the host `/usr`, `/usr/local`, `/etc` or provider home. Any unlisted file added to
the staged runtime causes launch refusal.

## Adding a declared provider

Use `/usr/bin/python3 scripts/provision-native-vm-probe.py --extend-policy
EXISTING_PRIVATE_POLICY --provider claude --native-executable EXACT_CLAUDE_BINARY
--auth-file EXPLICIT_AUTH_FILE --gateway-policy REVIEWED_CLAUDE_ROUTE_POLICY` to
add a previously undeclared provider. Supply `--environment-key NAME` separately
for every required environment credential or routing setting. The helper does
not infer new network destinations, copy environment values, replace existing
profiles, or alter the global resource policy.

Extension preserves the existing state directory, session aliases, credential
grants and provider routes. It verifies the old runtime manifest, stages a new
union with the additional executable's distribution dependencies, and publishes
one atomic policy pointer under a private provisioning lock. Existing runtime
generations remain available to active runs. Identical runtime files deduplicate;
a same-path byte conflict refuses publication and requires an explicit runtime
upgrade after compatibility checks. Failed staging removes only its unpublished
random generation. Retire old runtime generations only after their runs drain.

Codex and Claude native authentication require explicitly declared native
credentials. Routed Claude additionally needs its declared base URL and token;
routed Codex, Qwen and GLM use the Codex executable and a declared router key.
Each routed profile also needs reviewed gateway routes matching the actual
router endpoint. A router key is not native reviewer authentication. Profile
presence establishes eligibility only: authenticate and exercise the actual
executable, streaming, guarded tools and session continuation for each enabled
provider before rollout. The fixture-only provisioning tests prove profile
preservation and refusal boundaries; they do not make provider requests.

## Rollout through the normal updater

After the disposable authenticated acceptance probe passes, provision the
reviewed production profile outside every worker root. Use the same explicit
provider routes and credential grants proven in that probe. Keep the private
`state_root` stable across binary updates so observed native session aliases
continue to resolve. Runtime upgrades require a newly staged manifest and an
atomic policy update pointing to it; retain the prior runtime until old native
runs have drained. Do not copy a host home directory into a runtime image.

Select the reviewed native profile through the managed installer from the deployed
checkout. It writes the shared final environment file for both services and
preserves their launch commands:

```sh
AUGMENTAGENT_NATIVE_VM_POLICY=/absolute/private/policy.json \
  bash scripts/ensure-autopr-worker.sh
```

The installer validates all settings before replacing managed files and rolls
back the prior selection and units if installation or manager reload fails.
An `Environment=` drop-in cannot override this shared `EnvironmentFile=`.
The normal updater detects a changed selection or policy digest even when Git
and the build stamp are current. It activates the daemon through its usual
in-flight-work, memory, restart-budget and executable checks. The independent
worker notices selection and policy changes and reloads after its current
builder tick and reconciliation sweep finish. Activation remains pending when
a guard refuses; a later updater tick retries. Confirm both process-bound
policy digests with `autopr readiness --json` after they have drained.

This selects a profile only; it does **not** enroll run resources. A service
with `Delegate=no` or no `DelegateSubgroup=daemon` cannot admit required
Autoship work merely because this environment variable is present.

Use the configured `SYSTEMD_UNIT` (not a hardcoded unit name) and let
`scripts/check-for-updates.sh` perform its normal guarded update. Before its
verified restart, the updater invokes `configure-run-resources.sh --install`,
which installs delegation and run-resource settings and reloads the manager.
It preserves finite aggregate memory/task ceilings and stricter CPU limits.
Do not substitute `--apply`, a direct daemon restart, or an `ExecStart` override.
After restart, `configure-run-resources.sh --check` and protected actual run
admission/cleanup receipts are mandatory acceptance checks. An unsupported-host
status of 3 permits unrelated service updates but is **not** successful Autoship
enrollment. Follow the [rollout procedure](rollout.md) and retain its
[receipt template](rollout-receipt.template.json). The updater rebuild predicate includes every embedded VM,
gateway and MCP helper; no separate installed Python copy can stay stale. The
provisioning helper itself is read directly from the updated checkout. A missing
profile keeps required Autoship native calls closed while unrelated interactive
calls and ordinary updater operation retain their existing behavior.

The managed selection does not expand gateway grants or refresh the original
authentication file automatically. All profile paths are
controller inputs. This document describes the reviewed rollout artifact, not
an assertion that production has been configured or authenticated acceptance
has passed.

## Runtime retention and serialized credential use

The declared aggregate byte/session budget is checked under a private cross-
process lock before dispatch. Each retained session reserves a conservative
1 GiB upper bound for its executable, bounded input assets and runtime exports;
filesystem free space must also cover that reservation. Active/resumable
sessions are not discarded to meet the budget. Actual fresh reviewer/builder
scope retirement marks completed runtimes eligible for reclamation; a separate
explicit fenced workflow-owner retirement can release interrupted exports.
Reclamation requires the session execution lock to be free and removes only
controller-owned runtime generations and their native-session aliases.

The workflow controller must call `native_vm::retire_owned_runtimes` after
quiescent fenced recovery or terminal ownership resolution, using the protected
audit path plus workflow ID and fenced lease generation recorded in each runtime
ownership receipt. A later healthy lease may reuse an audit path; it is a
different owner and cannot retire that later lease by path alone. Bind the
logger once with `set_native_runtime_owner` after its fenced workflow claim. Age and PID absence are
not retirement authority. Capacity refusal persists the existing workflow
capacity dependency without admitting a model call; a subsequent retry prunes
newly retired state before evaluating that dependency again.

Refresh-enabled credentials have one held use lock across initial seed, native
execution, token refresh and descendant cleanup, not merely at file writeback.
Contention is a pre-dispatch capacity dependency. This prevents concurrent
refresh attempts from invalidating each other's tokens. The imported service
credential belongs to this controlled writer domain; do not concurrently run
another client against a copied refresh credential outside that protocol.

Runtime retirement is an outbox operation. The workflow store records the exact
owner and audit path before dispatch; terminal checkpoint transitions and
quiescent audit-generation recovery queue retirement in the same SQLite
transaction as the state transition. Free reconciliation retries the bounded
oldest-attempt window, including terminal workflows no longer eligible for a
builder. A crash after the pointer commit or after marker creation replays the
same owner without touching later claims. Missing VM policy leaves the intent
pending. Normal parked work does not authorize owner retirement.

The runtime store also persists an exact closed-owner tuple, so a late caller
cannot create a new session after that owner's cleanup was acknowledged.
Workflow model admission rejects terminal stages independently. Retirement
never removes protected audit or review evidence.

## Composed native and build memory

A guarded build VM is a descendant of the host MCP bridge and shares the native
provider's run cgroup. Admission sums the declared native guest RAM, any build
VM reachable through the bridge, and host overhead (default 1 GiB, explicitly
configurable with `AUGMENTAGENT_RUN_HOST_OVERHEAD_BYTES`). Without an explicit
run-memory override, the ceiling is the greater of this sum and the existing
4 GiB default. A 1 GiB native guest plus a 4 GiB build guest therefore requires
6 GiB. Read-only tool profiles do not reserve a build guest.

An explicitly configured smaller `AUGMENTAGENT_RUN_MEMORY_BYTES` fails with a
capacity dependency before model dispatch; it never resizes either guest.
Existing resource drop-ins retain their explicit settings, so an old 4 GiB
installation needs a reviewed update before running this combined workload.
Newly rendered defaults leave this setting unset to enable composition.

Admission serializes on the delegated service cgroup and counts the full
ceilings of other run groups, including idle guests, plus daemon usage. It also
checks ancestor limits and available host memory. The protected resource receipt
includes the reservation and effective limits. Reservations disappear only when
the corresponding cgroup is removed after descendant cleanup. This prevents
same-service lazy-RAM overcommit; unrelated host workloads can still change
memory availability after admission, and kernel limits remain authoritative.

The full native acceptance wrapper explicitly declares matching run and unit
budgets and checks host capacity before launching. For the 1+4 GiB guest case,
these are 6 GiB per run and 8 GiB for the isolated unit. The wrapper does not
change the production service or launch when that host headroom is unavailable.

The acceptance wrapper's preliminary host check uses the run reservation plus
an explicit controller allowance (`--controller-headroom-mb`, default/minimum
256 MiB) and a separate 256 MiB host margin. Thus the 6 GiB combined run can
start its controller at 6.5 GiB available memory while retaining the 8 GiB unit
ceiling. The allowance is an estimate, not a guaranteed controller memory bound.
Every real dispatch still passes RunScope's sibling, ancestor and host checks;
controller growth or other workloads may cause a subsequent capacity refusal.
The wrapper records these preliminary budgets with its result. A zero systemd exit
status alone is not a passing probe: stopping a service can return zero before
the test completes. The wrapper requires a fresh, private completion receipt
from the process supervising the exact test, plus its one-test passing summary.
An interrupted run without that receipt is failed/incomplete, even if systemd
reports success. Retain the transcript, result, and completion receipt together.


Probe budgets follow reachable capabilities: `issue` and `adoption` include the
configured build guest because they exercise builders/repairs. `writer` and
`adjudicator` only execute bounded controller Python observations and the
supplied-evidence native adjudicator, which clears tool grants before dispatch.
They therefore reserve the native guest plus host overhead (2 GiB for a 1 GiB
native guest), with a 4 GiB aggregate ceiling. This does not resize a guest or
change production composition: production still reads the actual bridge grants.
New probe names require an explicit capability classification.

The `routed_transport` choice also has no build grant: it exercises the actual
operator-routed native adapter with supplied text and no Bash. This checks that
transport's admission and inference path; it does not replace the full issue or
adoption pipeline acceptance runs.

Before starting a unit, the wrapper runs the selected managed binary's libtest
inventory with `--list --ignored --exact EXPECTED_TEST`. It requires exactly
that test and refuses stale binaries whose filters would run zero tests. The
receipt retains the tested binary digest and inventory-output digest; a binary
change between inventory validation and launch preparation also refuses launch.
Inventory inspection executes no test body or provider call.

An explicit `--defer-run-admission` probe option keeps that full-run estimate as
metadata but permits starting the isolated lightweight controller when available
memory covers only its allowance plus the host margin (512 MiB by default).
The default remains the stricter full-run estimate. Deferred mode preserves
both guest declarations, the full run reservation and aggregate ceiling; it
never reports the run admitted by preflight. Required RunScope admission still
occurs before native VM launch, and model billing occurs only after verified
VM readiness. Insufficient actual memory therefore refuses the run without a
model call. This option changes no production resource policy or process.

Resource admission failures retain fixed diagnostic codes in the protected
`run_resource_observation` receipt: `host_headroom`, `aggregate_reservations`,
`ancestor_headroom`, `declared_ceiling`, or `invalid_policy`. Numeric facts show
requested/reserved/current/available bytes and applicable limits/margins.
Unclassified setup exceptions retain only an allowlisted class and numeric
errno. No exception message, raw stderr, filename, credential or policy payload
is copied. These diagnostics do not change refusal, billing or retry semantics.

OAuth rotation uses a host checkpoint, independently of normal conversation
export. With the explicit refresh grant, the gateway recognizes only the exact
Claude `POST https://platform.claude.com/v1/oauth/token` or Codex
`POST https://auth.openai.com/oauth/token` endpoint and the supported native client
ID. This recognition **does not add a network route**; the operator must still
declare the exact route in the gateway policy. Encoded/query variants, other
clients, expanded requested scopes and non-refresh grants are refused. Codex
0.164.0 optional response tokens preserve their previous values when omitted or
null; account identity checks still apply. Custom issuer/client overrides require
separate reviewed support rather than silently bypassing the checkpoint.

Before upstream dispatch the controller durably records a pending refresh intent.
A bounded successful response is checkpointed privately and validated, then the
controlled credential is fsynced before any response headers/body reach the guest.
A received-response journal recovers across controller death before credential
publication; replay preserves the original receipt timestamp and token expiry.
Guest cancellation lets an in-flight refresh finish its checkpoint within the
existing upstream timeout. A new session uses the updated shared credential even
when no normal guest runtime export exists. No refresh payload appears in public
receipts or logs; the temporary private response journal is replaced by digest-only
completion metadata after successful credential persistence.

Issuer rotation followed by loss of the response before durable receipt remains
an externally uncertain OAuth outcome. Pending outcomes (including ambiguous HTTP
failures) refuse both same-launch retries and subsequent launch admission instead
of declaring the old token healthy. Recovery requires issuer/account reconciliation
or a separately authorized credential reimport with explicit journal reconciliation;
we do not automatically reuse a potentially consumed refresh token. The synthetic
non-network tests cover rotation followed by SIGKILL and fresh-session seeding,
received-response recovery, original expiry, concurrent/stale requests, scope
refusal, and persistence-before-delivery. Natural provider refresh and fresh-session
authentication remain distinct rollout checks; these tests use no actual credentials.

## Read-only credential diagnostics

`augmentagent autopr readiness --json` reports native credential declarations for
Claude and Codex without invoking a provider or refreshing credentials. It reads
the selected private policy each time, includes its SHA-256, and reports missing
policy, invalid policy, undeclared route, missing/invalid credential, known
expiry, unknown expiry, explicitly authorized refresh, or an unexpired/static
credential declaration. It never prints credential values.

Claude subscription expiry uses the credential's `expiresAt` milliseconds;
Codex subscription expiry reads the access token's JWT `exp` seconds. Reading
that field does not verify the token's signature, revocation status, quota, or
provider reachability. Opaque tokens retain an explicit unknown-expiry status.
A known expired token is ineligible for protected native dispatch unless the
existing profile authorizes refresh of its imported controller credential.
This does not enable refresh or modify a credential grant.

Credential declarations alone cannot establish workflow readiness. The command
keeps `ready: false` and names the role, resource, deployment and live-probe evidence
that remains unverified; exit 3 means readiness has not been established.


The readiness report also verifies the selected native guest's complete manifest
and reports its digest, file count and byte count. It rejects missing or changed
artifacts, path traversal, duplicate entries, symlinks, extra files and an unsafe
guest root. Hashing streams bounded reads; inspection never repairs files or
retires runtime generations. Every invocation rechecks the inventory.

Build-runtime inspection validates the private configuration, required fields,
memory range, module declarations and trusted artifact/directory permissions.
It reports the configuration digest and individual hashes for QEMU, kernel,
BusyBox, firmware and modules. Dependency directory contents are not certified
by these hashes; the guest launch and acceptance probes retain their checks.
`build-declaration-verified` is a declaration check, not evidence that a VM booted.

Non-dry-run `run_once` calls this read-only preflight before acquiring a run lock,
creating workflow state, inspecting Git or selecting issues. Missing/invalid
declarations return a `native-runtime:` blocker. Hashing runs off the async
executor, and the actual launch independently revalidates its declaration.

Provider compatibility is checked separately from the guest inventory. The
readiness report hashes the currently resolved provider executable. For a Codex
executable, the runtime must declare its matching `codex-code-mode-host`, and
that companion must appear in the file manifest with matching bytes. A Codex
upgrade can invalidate this mapping even when every old guest artifact still
matches its manifest. Protected provider admission rechecks this compatibility,
including calls through an already-constructed chain.

Use the existing runtime-only provisioning operation to prepare a new generation:

```sh
python3 scripts/provision-native-vm-probe.py \
  --refresh-runtime-policy /absolute/controller/policy.json \
  --native-executable /absolute/path/to/codex
```

This preserves credentials, grants, resources and state paths and retains the
old generation. It changes only the guest-runtime pointer after syncing the new
artifacts. Do not delete old generations while leases may still use them. Validate
the refreshed generation with the current packaged guarded-edit/review probes;
a compatibility hash alone does not prove provider execution.

The normal updater runs `scripts/ensure-native-runtime.py` before its unchanged-Git
exit and again after applying an update. It acts only on the explicit private
`autoship-native.env` selection described above. It resolves Codex using the
running auto-PR worker's `CODEX_CLI` and `PATH`, falling back to the daemon and then
the updater environment if neither service is running. It never imports credentials
or activates a profile. Current generations are validated and left untouched;
stale companion mappings cause a runtime-only refresh under the provisioning lock.
Corrupt inventories fail with `native-runtime-reconciliation-failed` instead of
being silently replaced. Failures are logged and retried on the next updater tick
without blocking software updates needed to repair them. A successful binary
build stamp does not assert native provider readiness.

Refresh retains previous generations. If publication is attempted but reports an
error, it also retains the new generation: a workflow could already have observed
that pointer, even if another actor subsequently changes it. Reclamation must
respect runtime ownership and leases; pointer absence alone is insufficient.

Read-only `autopr readiness --json` also reports `roles.builder`,
`roles.independent_reviewer` and `roles.adjudicator`. Builder admission uses the
same effective model selection, routing, capability, credential and cooldown
filters as the workflow chain. Review admission considers all currently available
builders and the configured native reviewer pool; native Codex can supply the
existing protected fresh-session review path. Actual review still requires durable
authorship and its controller-owned conversation receipt. Final adjudication
requires native Codex separately, even when another provider can build or review.
Missing capacity and cooldowns stay distinct from declaration availability.

A non-dry-run worker checks these roles after runtime inspection and before
creating a run lock, inspecting Git or selecting an issue. This catches a usable
builder with no review/adjudication path before scoping spends model calls. Reads
of model selection observe an atomic published snapshot without creating selection
locks or directories. Providers and credentials are rechecked on later admission;
`declared-available` never asserts a successful live call, sufficient resources,
matching deployed configuration, or permission to merge.

Resource readiness is reported separately under `resources`. It composes the
native guest, build guest and declared host overhead with the same memory rule
used at launch, then checks the calling process's delegated service and aggregate
ceilings through the packaged resource supervisor. A shared nonblocking admission
lock protects the memory-capacity snapshot. Existing idle `run-*` reservations
remain charged at their full ceilings. Invalid declarations, an undersized service,
missing delegation, exhausted memory headroom and a busy inspector have distinct
statuses. Inspection has a five-second subprocess deadline and creates no helper
file, cgroup, workflow state or reservation; it never reclaims cache.

An unused delegated service can report `capacity-unverified` with
`controller-initialization-required`. That permits the existing locked admission
step to initialize accounting and attempt reservation; it does not authorize model
execution or claim readiness. Other resource blockers stop the non-dry-run worker
before Git access or issue selection. Every actual launch still reserves capacity
and verifies limits before provider admission. The snapshot is not a reservation,
and CPU/PID ceilings are declarations, not a promise of future throughput.
A CLI run outside a delegated service reports its own missing delegation; it does
not infer the serving worker's effective configuration or capacity.

## Guarded file-edit probes for each native provider

Build the CLI test artifact through `scripts/cargo-managed.sh`, then run either
`file_edit` (Codex) or `claude_file_edit` with
`scripts/autoship-native-acceptance-probe.py PROBE --binary MANAGED_TEST_BINARY
--vm-policy PRIVATE_POLICY`. Each entry point uses the normal pinned provider
constructor and the operator's configured routing. Do not remove declared
environment routing keys to make a transport-only helper accept the profile.

Both probes require the exact file change, an observed native conversation,
matching workflow ownership, a guarded Write/Edit receipt, exactly one model
dispatch, and completed descendant quiescence. Claude's guarded MCP audit names
use `mcp__jarvis__`; Codex's bridge records canonical names. The verifier accepts
only the provider's exact namespace and the five declared file tools. Shell and
foreign MCP names remain forbidden. These probes do not authorize a build, prove
native continuation, exercise independent review, or establish issue-to-merge
acceptance. The full workflow probes remain separate.
