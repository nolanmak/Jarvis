# Required-workflow native VM transport

Required autoship loggers select the common VM launcher for Claude, Codex and the
Codex-backed Qwen/GLM adapters. Missing policy, missing provider profile or an
incomplete saved native runtime fails before native dispatch. Interactive calls
without the required-workflow marker retain their existing transport.

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

## Rollout through the normal updater

After the disposable authenticated acceptance probe passes, provision the
reviewed production profile outside every worker root. Use the same explicit
provider routes and credential grants proven in that probe. Keep the private
`state_root` stable across binary updates so observed native session aliases
continue to resolve. Runtime upgrades require a newly staged manifest and an
atomic policy update pointing to it; retain the prior runtime until old native
runs have drained. Do not copy a host home directory into a runtime image.

Configure the existing daemon unit with one environment-only drop-in, preserving
its `ExecStart` and aggregate resource settings:

```ini
[Service]
Environment=AUGMENTAGENT_NATIVE_VM_POLICY=/absolute/private/policy.json
```

Use the configured `SYSTEMD_UNIT` (not a hardcoded unit name), reload the user
manager, and let `scripts/check-for-updates.sh` perform its normal deferred,
verified restart. The updater rebuild predicate includes every embedded VM,
gateway and MCP helper; no separate installed Python copy can stay stale. The
provisioning helper itself is read directly from the updated checkout. A missing
profile keeps required Autoship native calls closed while unrelated interactive
calls and ordinary updater operation retain their existing behavior.

No installer in this patch writes that drop-in, expands gateway grants or
refreshes the original authentication file automatically. All profile paths are
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
