# AugmentAgent browser sidecar

Python asyncio sidecar that owns the long-running Chromium attached over CDP.
The Rust daemon talks to it via NDJSON over `${XDG_RUNTIME_DIR}/augmentagent/browser.sock`.

Implements the browser-sidecar spec (originally `#75` in the archived
`nolanmak/AugmentAgent` repo; that number does not map to the same issue in
the canonical [nolanmak/MyAgentAssistant](https://github.com/nolanmak/MyAgentAssistant)
repo). Foundation crate: `crates/augmentagent-browser-client`.

## Layout

```
sidecars/browser/
  sidecar.py        # asyncio Unix-socket server, 13 ops, typed errors
  requirements.txt  # playwright==1.49.1 + browser-use==0.12.6
  pyproject.toml    # editable-install metadata
  setup.sh          # one-shot venv + playwright install chromium
```

Three systemd units in `systemd/`:

- `augmentagent-xvfb.service` — `Xvfb :99 -screen 0 1600x1200x24 -ac`
- `augmentagent-chromium.service` — headed Chromium with
  `--remote-debugging-port=9223 --remote-debugging-address=127.0.0.1
  --user-data-dir=…/browser-profile`
- `augmentagent-browser-sidecar.service` — runs `sidecar.py`, depends on the
  above two.

## Setup (one-time per host)

```bash
# 1. python venv + playwright + bundled chromium (~500 MB)
./sidecars/browser/setup.sh

# 2. install + start the three systemd units
mkdir -p ~/.config/systemd/user
cp systemd/augmentagent-xvfb.service \
   systemd/augmentagent-chromium.service \
   systemd/augmentagent-browser-sidecar.service \
   ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now \
    augmentagent-xvfb.service \
    augmentagent-chromium.service \
    augmentagent-browser-sidecar.service

# 3. verify all three are active
systemctl --user is-active \
    augmentagent-xvfb augmentagent-chromium augmentagent-browser-sidecar
```

## One-time login flow (per platform: IG / LinkedIn / X)

The sidecar drives Chromium **headlessly from the user's perspective** but the
browser itself is *headed* on Xvfb display `:99`. Cookies live in
`~/.local/state/augmentagent/browser-profile/Default/Cookies` and survive
restarts. You only need to log in once per platform — until cookies expire or
2FA "trusted device" markers roll over.

### Option A: VNC over SSH (recommended, no extra services on the box)

Install `x11vnc` on the box, attach it to the existing Xvfb display, and
tunnel the loopback port over SSH from your workstation.

```bash
# On the box (one-time install)
sudo apt install -y x11vnc

# On the box (start an attach session — runs in foreground, Ctrl-C when done)
DISPLAY=:99 x11vnc -display :99 -localhost -nopw -forever -shared -rfbport 5900

# On your workstation: forward the loopback VNC port over SSH
ssh -L 5900:127.0.0.1:5900 nolan-makatche@<box-host>

# Still on your workstation: open VNC against the local end of the tunnel
vncviewer 127.0.0.1:5900
# (or any VNC client: TigerVNC, RealVNC viewer, macOS "Screen Sharing.app",
#  Remmina, etc.)
```

You'll see Chromium at full 1600×1200. Navigate to:

- `https://twitter.com/login` — complete 2FA, tick "remember this device"
- `https://www.linkedin.com/login` — same
- `https://www.instagram.com/accounts/login/` — same

Wait ~30 seconds after the last login (Chromium flushes cookies on a timer)
before closing the VNC session. Then `Ctrl-C` the `x11vnc` process and tear
down the SSH tunnel.

### Option B: noVNC in a browser (no native VNC client needed)

Future-tracked as a separate UX issue (#75 §"Out of scope"). Until then,
install `novnc` + `websockify` on the box and proxy `:6080 → :5900`; same
SSH-tunnel pattern, just the browser instead of `vncviewer`.

### Verifying the cookie jar is healthy

```bash
# Should print PASS and write a screenshot.
cargo run -p augmentagent-cli -- browser acceptance-test
# Or directly via the Python:
.venv/bin/python sidecars/browser/sidecar.py &  # if not already up
.venv/bin/python -c "
import asyncio, json, uuid, base64
from pathlib import Path
async def main():
    r, w = await asyncio.open_unix_connection('/run/user/1000/augmentagent/browser.sock')
    req = {'request_id': str(uuid.uuid4()), 'op': 'navigate',
           'params': {'url': 'https://twitter.com/home'}, 'timeout_ms': 30000}
    w.write((json.dumps(req)+chr(10)).encode()); await w.drain()
    print(await r.readline())
asyncio.run(main())
"
```

## Wire protocol

NDJSON over Unix stream socket. See `sidecar.py` module docstring for the
exact request/response envelope and `crates/augmentagent-browser-client/src/lib.rs`
for the Rust side.

Ops: `ping`, `navigate`, `click`, `type`, `screenshot`, `get_text`,
`set_input_files`, `wait_for`, `evaluate`, `count`, `press_key`, `drag`,
`bounding_box`.

Typed error kinds: `AuthRequired`, `CaptchaDetected`, `ChromiumDisconnected`,
`Timeout`, `SelectorNotFound`, `Navigation`, `Internal`. The first two carry a
`page_url` + base64 `screenshot_b64` so the Discord approval card has
something to show the operator.

## Operational notes

- The sidecar attaches to Chromium **lazily** — it doesn't fail to boot if
  Chromium is down; the next request returns `ChromiumDisconnected`. systemd
  restarts Chromium → next request reattaches.
- Single shared `Page` per sidecar process. Sequential ops are serialized via
  `asyncio.Lock`; concurrent ops on multiple connections are fine.
- The bundled Chromium that `playwright install chromium` downloads is **not**
  the long-running browser — that comes from the system `/usr/bin/chromium`
  package via `augmentagent-chromium.service`. The bundled one only ships the
  protocol rev so Playwright knows what to speak over CDP.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `ChromiumDisconnected` on every call | `systemctl --user status augmentagent-chromium` — likely Xvfb or chromium failed |
| `AuthRequired` on a site we used to be logged into | cookies expired or 2FA "trust" rolled over — repeat the §"One-time login" flow |
| `socket file does not exist` | `systemctl --user status augmentagent-browser-sidecar` — sidecar didn't start; check logs at `~/.local/state/augmentagent/browser-sidecar.log` |
| `playwright: command not found` after pulling | re-run `sidecars/browser/setup.sh` |
