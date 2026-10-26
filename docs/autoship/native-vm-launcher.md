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
  "state_root": "/controller/private-native-vm",
  "memory_mb": 1024,
  "providers": {
    "codex": {
      "auth_file": "/controller/provider/auth.json",
      "auth_relative": ".codex/auth.json",
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

The guest exports no NIC and receives only read-only system runtime, one native
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
changed independently. Propagation of refreshes back into original credentials
is not implemented; credential freshness for new conversations must be proven
in the authenticated rollout test.

Local executable validation:

- `python3 scripts/test-native-provider-vm.py`: four tests cover pre-admission
  refusal, cancellation cleanup, completed-generation recovery and symlink
  rejection. These do not execute a VM or contact a provider.
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
