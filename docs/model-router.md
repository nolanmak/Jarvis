# Optional OmniRoute gateway

Native CLI accounts are the default. OmniRoute is an optional account gateway;
installing it does not turn routing on. Production remains in `mode: direct`.
Follow [the pinned Linux installer, prerequisites and rollback procedure](omniroute.md).
The installer is an explicit operator command, not part of daemon startup or
normal updates. The macOS service installer is deferred.

## Routing and credentials

`$XDG_CONFIG_HOME/augmentagent/model-router.json` (normally
`~/.config/augmentagent/model-router.json`) chooses the gateway and mode.
`AUGMENTAGENT_MODEL_ROUTER_CONFIG` can select another owner-private path.
The config contains `gateway: omniroute`, `mode: direct`, a loopback `/v1`
base URL and the inference API key. The mode values `auto`, `claude` and
`codex` are explicit operator choices. Do not set them during installation,
rollout, timer activation, or health recovery.

Service secrets are in the system keyring and a mode-0600 `omniroute.env`;
management tokens are distinct from the inference key. Data lives outside the
repository under `$XDG_DATA_HOME/augmentagent/omniroute`. Secrets, account
tokens and database backups never belong in a wiki mirror or source checkout.
The default service is `augmentagent-omniroute.service`, headless and bound to
127.0.0.1:20128 only. Public tunnels are disabled.

A reasoner call snapshots its routing config. Changes affect the next call,
while an in-flight call retains its route. Claude models stay within `cc/…`
and Codex models within `cx/…`; a gateway account name does not establish
independent reviewer identity. Independent review stays on native transport.
Global compression, Caveman and RTK are disabled by the installer.

## Operator workflow and release gates

Use `scripts/install-omniroute.sh` only after reviewing its prerequisites.
It verifies `scripts/omniroute.pin`, provisions private credentials, checks
loopback health, and leaves routing direct. `--upgrade` takes a supported CLI
backup and attempts to restore the prior package/data after failed health. If rollback
fails, inspect the service before relying on the prior installation.
`--uninstall` retains data; `--uninstall --purge` is an explicit destructive
choice for owned state. See [the complete commands](omniroute.md).

Gateway operations (#1665), health polling and the opt-in signed receiver
(#1658), the dashboard adapter (#1657), and end-to-end tool/capability smoke
(#1659) are separate delivery gates. An installer health check or a model
catalog entry does not prove end-to-end tool use. Keep routing direct until
those gates pass. Runpod support remains deferred under #1661.

The old patched-image CI gate is retired with its source. The current pinned
installer tests use disposable service/API fixtures and verify direct defaults,
private credentials, checksum/Node checks, loopback-only startup, idempotence,
and backup/restore. They do not claim a live OmniRoute package smoke or actual
host acceptance; #1659 owns that replacement integration gate.

```sh
python3 -B -m unittest discover -s scripts/tests -p omniroute_install_test.py -v
python3 -B -m unittest discover -s scripts/tests -p model_router_retirement_test.py -v
```

## Migrating an existing private configuration

The installer refuses a conflicting active configuration, including one already
set to direct. Preserve the private config, atomically disable its route, then
move it out of the active path before explicitly installing OmniRoute. Existing
in-flight calls retain their snapshot; subsequent calls use native CLI accounts
when the active file is absent. This does not migrate account tokens or enable
a gateway route. Run from the repository as the owning user:

```sh
python3 - <<'PYTHON'
import datetime, json, os
from pathlib import Path
from scripts.lib.omniroute_install import private_read, private_write
config_root = Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config'))
source = Path(os.environ.get('AUGMENTAGENT_MODEL_ROUTER_CONFIG', config_root / 'augmentagent/model-router.json'))
original = private_read(source)
stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')
private_write(source.with_name(source.name + '.pre-migration-' + stamp), original)
value = json.loads(original)
value['mode'] = 'direct'
private_write(source, json.dumps(value, indent=2) + '\n')
os.replace(source, source.with_name(source.name + '.disabled-' + stamp))
PYTHON
# Review prerequisites first; this remains an explicit operator action:
scripts/install-omniroute.sh
```

Both preserved configurations stay mode 0600 outside the repository. Keep the
old service's private account data until the owner confirms migration. Dashboard
account management awaits the compatible adapter (#1657); installation alone
does not complete account setup. Use native CLI accounts/direct mode meanwhile.

## Legacy retirement

**9Router (removed).** The installer, launcher, image builder, patched sidecar and their obsolete image/lifecycle fixtures were removed after the OmniRoute gateway kind and installer landed; see [#1652](https://github.com/nolanmak/Jarvis/issues/1652) and [#1660](https://github.com/nolanmak/Jarvis/issues/1660). The optional serialized compatibility kind remains for existing private configurations, without provisioning or starting it. Host cleanup is an owner-only step: inspect `systemctl --user cat 9router-mini-tunnel.service`, run `systemctl --user disable --now 9router-mini-tunnel.service` if present, remove its reviewed user-unit file/drop-ins, then `systemctl --user daemon-reload` and confirm it is inactive; this PR does not change production services, delete private account data, or alter the operator's interactive `claude-9router` shell wrapper. Historical patch receipts are not OmniRoute acceptance evidence; git history retains the removed source.
