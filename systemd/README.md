# systemd units — browser sidecar

Three user-mode units that wire up the headed-Chromium-on-Xvfb stack the
browser sidecar attaches to. Install pattern matches the existing
`scripts/install-autostart.sh` flow (units land in `~/.config/systemd/user/`,
no root required).

| Unit | Description |
|------|-------------|
| `augmentagent-xvfb.service` | Virtual display `:99` at 1600×1200×24 |
| `augmentagent-chromium.service` | Headed Chromium with `--remote-debugging-port=9223` and a persistent `--user-data-dir` |
| `augmentagent-browser-sidecar.service` | Python sidecar (`sidecars/browser/sidecar.py`) — Unix socket at `${XDG_RUNTIME_DIR}/augmentagent/browser.sock` |

## Install

```bash
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
```

Then complete the one-time login flow documented in
`sidecars/browser/README.md`.

## Optional: gate the Rust daemon on the sidecar

Add to `~/.config/systemd/user/augmentagent.service` under `[Unit]`:

```
After=augmentagent-browser-sidecar.service
```

`After=` (not `Requires=`) — the daemon must still boot when the sidecar is
down so non-browser channels keep working.
