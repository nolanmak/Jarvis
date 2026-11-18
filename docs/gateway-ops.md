# Owner gateway operations

OmniRoute remains optional and routing remains **direct by default**. No deploy,
timer, health recovery or normal daemon startup calls `gateway on`.

```sh
augmentagent gateway status --json
augmentagent gateway off --json
augmentagent gateway on --mode auto     # also claude or codex; owner CLI only
augmentagent gateway update            # apply reviewed scripts/omniroute.pin
augmentagent gateway update --to 3.8.52 # must match that pin, including its sha512
augmentagent gateway rollback
```

Commands return structured JSON and a nonzero status on failure. They run before
application database bootstrap. A missing optional config is a successful no-op
for `off`. Config/state comes from XDG paths and `AUGMENTAGENT_MODEL_ROUTER_CONFIG`;
repository lookup uses `AUGMENTAGENT_REPO_ROOT` or the working directory. Updates
require the real repository pin; a compiled-in pin supports read-only commands
when the binary is used outside a checkout.

`off` changes only `mode` to `direct`, atomically, and saves a private
`.bak-<mode>-<UTC>-<nonce>` config beside the active file. All other fields and
secrets are preserved. The service stays running. An in-flight call retains its
existing routing snapshot; the next call reads direct mode. Config backups stay
mode 0600 and must not enter the repository/wiki.

`status` reports gateway kind, mode, local managed-unit activity, `/api/health`,
authenticated `/api/monitoring/health` quota/breaker counters, provider expiration
counts, and daemon breaker observation. It uses the installer keyring's read
token and emits neither credentials nor account identities. Any error/exhausted
count, expired credential, open breaker, or unavailable observation is red.
Remote gateway health uses the configured URL; local service updates and
rollback refuse remote instances rather than SSHing to them.

**Daemon breaker dependency:** #1655 has no shared observation implementation
on the base revision. Its adapter explicitly reports `unavailable`; `status`
and `on` therefore cannot falsely claim green. Wire the actual process-shared
breaker contract before enabling a gateway. A missing entry is not silently
interpreted as `closed`.

`on` requires green status, a successful `reasoner-selftest --routed` against the
selected gateway provider, and another green status before changing the mode.
The probe uses a private temporary config; it never transiently enables routing
for other callers. Native CLI rescue cannot satisfy a routed probe, while
ordinary reasoner calls keep their existing rescue behavior.

`update` verifies the pin/package, takes `omniroute backup`, switches direct,
stops the owned unit, and applies the pinned npm package. Health and routed
selftest run before the transaction commits. The previous route is restored
only when it was enabled and status is green. Failures retain direct mode and
attempt supported prior-package + `omniroute restore <id> --yes` rollback;
errors emit one registered owner maintenance notification. If fetching or
validating a pin fails before package changes, the old package stays running
and the route still switches direct. `--to` cannot bypass the reviewed integrity
pin or fetch an arbitrary release.

One verified previous package, previous pin and corresponding backup ID are
retained for `rollback`. Rollback leaves the route direct; only a subsequent
explicit, gated `on` can enable it. Backup directories remain in OmniRoute's
supported format; no raw database copies are made. Failed rollback needs
operator inspection and is never reported as success.

Owner Discord text commands `/gateway status` and `/gateway off` use the same
owner admission/interception as `/model`, before history or model inference.
Gateway-prefixed attempts to enable/update/rollback from chat are consumed and
refused. Slack reports its named parity block. The interactive gateway shell
wrapper and remote-host service management are outside this command's scope.

Pinned v3.8.52 contract: `circuitBreakers` is an aggregated object;
`quotaMonitor.statusCounts` is nested; `/api/providers/expiration` returns
`{summary,list}` and the summary uses **`expiringSoon`**. Management requests
use the separate read-scope token, not the inference key. Supported backup
commands are `omniroute backup` and `omniroute restore <id> --yes`. See the
[tagged source links](omniroute.md#pinned-contract-verified-against-v3852).
