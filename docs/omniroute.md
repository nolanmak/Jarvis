# Optional OmniRoute installation

OmniRoute is optional. Nothing in daemon startup, the updater, or this change
installs or enables it. Production remains in `mode: direct`. This installer is
an explicit operator action on a Linux host with a systemd user session; a
macOS service variant is deferred. Do not run it on the Legion as part of lane
validation.

## Prerequisites and explicit installation

Use Node **>=22.22.2 <23 or >=24 <27**, npm, Python 3.9+, `ss` (iproute2),
`systemctl`, `systemd-analyze`, and `secret-tool` (libsecret-tools), with an
unlocked Secret Service keyring reachable from the installing user's session.
A Node upgrade is a separate operator step; the installer refuses older Node
rather than changing it or rebuilding other services. Run as the same user as
AugmentAgent, without sudo. A missing/locked keyring fails closed.

```sh
scripts/install-omniroute.sh
```

The installer reads the version and npm sha512 SRI from **one file**,
`scripts/omniroute.pin`. It downloads that exact npm release tarball, verifies
its sha512 **before any installation**, and passes the verified tarball to
`npm install -g` with scripts enabled. `OMNIROUTE_SKIP_POSTINSTALL` is removed
from the installation environment so native modules warm up. It never installs
`latest` or changes the pin itself. The global npm prefix must be writable by
this user. An existing unmanaged OmniRoute installation or conflicting router
configuration is refused; migration must be explicit.

The owned user unit is `augmentagent-omniroute.service`, running
`omniroute serve --headless --port 20128`, with `OMNIROUTE_HEADLESS=1`,
`OMNIROUTE_SERVER_HOST=127.0.0.1`, `REQUIRE_API_KEY=true`, `Restart=on-failure`,
and a private umask. `ss -H -ltn` must show **only 127.0.0.1:20128** for that
port; wildcard and IPv6/public listeners fail verification and stop the new
service. Systemd unit validation precedes startup. The unit is enabled only
after authenticated health checks pass.

Default paths (XDG roots are honored):

| Purpose | Path |
| --- | --- |
| Data | `$XDG_DATA_HOME/augmentagent/omniroute` (`~/.local/share` by default) |
| Service environment | `$XDG_CONFIG_HOME/augmentagent/omniroute.env` |
| Install receipt | `$XDG_CONFIG_HOME/augmentagent/omniroute-install.json` |
| Client configuration | `$XDG_CONFIG_HOME/augmentagent/model-router.json` |
| Unit | `$XDG_CONFIG_HOME/systemd/user/augmentagent-omniroute.service` |

Data is outside the repository/wiki mirror. Repository-contained data paths
are refused, including symlink resolution. Do not add it to a wiki sync root.
The env, receipt, and router configuration files are mode 0600; the data root is
0700. Symlinked or non-private preexisting credential/config files are refused.

Secret Service records use attributes `service=augmentagent` and
`account=omniroute/<name>`: `INITIAL_PASSWORD`, `JWT_SECRET`, `API_KEY_SECRET`,
`inference`, `read`, and `write`. Secrets are generated once, stored/read back
from the keyring, then mirrored only into the private files that need them.
The management password never appears in the client router config. After the
first authenticated login persists its hash, `INITIAL_PASSWORD` is removed
from the unit env file and the service restarted. Its keyring record remains
for authenticated operator maintenance. Secret values are never printed or
passed as command-line arguments.

First boot authenticates with the management password, keeps login enforcement
on, and verifies cloud sync and all three tunnel providers are off. It refuses
an already exposed/tunneled instance rather than changing unrelated host
Tailscale configuration. It explicitly disables global compression, automatic
compression, Caveman (including output mode), RTK, stacked engines, and combo
overrides, then reads back the disabled settings. It creates one inference API
key (no management scope), plus separate **read** and **write** scoped management
tokens for #1658/#1657. Reruns reuse the keyring credentials and do not mint more
keys/tokens, reinstall npm packages, or restart an already healthy service.

`model-router.json` is created with `gateway: omniroute`, **`mode: direct`**, and
`base_url: http://127.0.0.1:20128/v1`. Compatible existing direct-mode settings
are preserved. Existing automatic routing or a legacy gateway configuration is refused
rather than silently redirected. Initial model aliases follow the existing
router installer defaults; connecting accounts and selecting supported routes
remain separate setup steps.

The final output names `augmentagent gateway status` and `augmentagent gateway
on` for the owner. Those CLI commands are delivered by #1665; the installer does
not pretend they already exist or invoke `gateway on`. Until that lands, inspect
the service and direct-mode config using the commands below.

```sh
systemctl --user is-active augmentagent-omniroute.service
ss -H -ltn
curl --fail http://127.0.0.1:20128/api/health
curl --fail http://127.0.0.1:20128/api/health/ping
# /api/monitoring/health requires management auth; /v1/models requires the
# inference key. The installer checks both without exposing either credential.
```

## Pinned contract verified against v3.8.52

The issue's historical examples have been checked against the tagged source
and [npm package metadata](https://registry.npmjs.org/omniroute/3.8.52).
Verification sources and differences:

| Contract | Tagged source / decision |
| --- | --- |
| Node floor and bins | [`package.json`](https://github.com/diegosouzapw/OmniRoute/blob/v3.8.52/package.json) |
| Loopback bind | [`bin/cli/utils/serverHost.mjs`](https://github.com/diegosouzapw/OmniRoute/blob/v3.8.52/bin/cli/utils/serverHost.mjs): `OMNIROUTE_SERVER_HOST` is the explicit Unix setting; shell `HOSTNAME` is ignored |
| Headless serve | [`bin/cli/commands/serve.mjs`](https://github.com/diegosouzapw/OmniRoute/blob/v3.8.52/bin/cli/commands/serve.mjs) |
| Login | `POST /api/auth/login {password}` returns the `auth_token` cookie |
| Inference key | `POST /api/keys {name,scopes:[]}` returns `key`; upstream adds `self:usage`, no `manage` scope |
| Management tokens | [`POST /api/cli/tokens`](https://github.com/diegosouzapw/OmniRoute/blob/v3.8.52/src/app/api/cli/tokens/route.ts) with `scope: read` or `write` returns the once-only `token`; these replace the earlier broad manage-key sketch |
| Compression | [`GET/PUT /api/settings/compression`](https://github.com/diegosouzapw/OmniRoute/blob/v3.8.52/src/app/api/settings/compression/route.ts), with `enabled:false`, modes `off`, `cavemanConfig`, `cavemanOutputMode`, and `rtkConfig` disabled |
| Monitoring compression | `/api/monitoring/compression` reports memo statistics, **not** the global mode; readback uses settings/compression |
| Cloud/tunnels | `GET /api/settings` exposes `cloudEnabled`; GET `/api/tunnels/{cloudflared,ngrok,tailscale}` exposes running state; fresh defaults must remain off |
| Backup/restore | [`bin/cli/commands/backup.mjs`](https://github.com/diegosouzapw/OmniRoute/blob/v3.8.52/bin/cli/commands/backup.mjs): bare `omniroute backup` creates a backup; `omniroute restore <id> --yes` restores without an interactive prompt |

The owner defaults are used: the `augmentagent-omniroute.service` name, one pin
file, CLI backup/restore. A webhook is **not registered by this installer**:
#1658 ships polling first and only enables a signed receiver after its HMAC
secret is provisioned/validated. No unsigned or unconfigured receiver is enabled.

## Updates and rollback

A maintainer changes version **and sha512 together** in `scripts/omniroute.pin`
after verifying that release's contract. Then the operator explicitly runs:

```sh
scripts/install-omniroute.sh --upgrade
```

Without `--upgrade`, a changed pin is refused. Before updating, the installer
verifies the retained prior package against its receipt, stops its owned unit,
runs **`omniroute backup`**, and verifies that exactly one new backup manifest
exists. No raw database copies are created. If backup fails, the old service is
restarted and no npm update occurs. On a failed update/start/health/bootstrap,
the installer stops the service, reinstalls the prior verified package, runs
**`omniroute restore <backup-id> --yes`**, restarts, and verifies health. A
successful rollback is reported as an installation failure, so automation
cannot mistake the failed update for success. A rollback failure remains a
failure requiring operator inspection; routing remains direct.

The installer retains one verified package artifact under `DATA_DIR/installer`
for the next rollback. OmniRoute owns its backup directories/format; backup
retention/upgrade policy beyond this transaction is #1665.

```sh
# Remove service and global package, retaining data/config/keyring credentials:
scripts/install-omniroute.sh --uninstall
# Explicit destructive variant removes owned OmniRoute data/config/credentials:
scripts/install-omniroute.sh --uninstall --purge
```

Uninstall requires an owned installation receipt. It never changes the host's
Node installation or unrelated services. The obsolete gateway installer and sidecar sources were removed by #1660;
see the retirement note in [model-router.md](model-router.md).

## Validation

`python3 -B -m unittest discover -s scripts/tests -p omniroute_install_test.py -v`
uses real temporary directories and a disposable command/API host model on the
mini; the HTTP transport test uses a real loopback HTTP fixture. It covers
fresh bootstrap, private credentials, direct defaults, listener checks,
idempotence, checksum refusal, Node floors, backup-before-update, rollback,
backup failure, uninstall retention, purge isolation, and redirect refusal.
It does not install OmniRoute or claim a live Linux systemd smoke ran on the
mini or Legion. A Linux operator smoke is an explicit later rollout action.
