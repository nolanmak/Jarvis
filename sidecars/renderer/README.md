# AugmentAgent renderer sidecar

Node sidecar that owns a long-running [Remotion](https://www.remotion.dev/)
bundle and renders a parametrized vertical (1080×1920) video from JSON props.
The Rust daemon talks to it via NDJSON over
`${XDG_RUNTIME_DIR}/augmentagent/renderer.sock` — the **same wire envelope** as
the browser sidecar (`sidecars/browser/sidecar.py`).

Phase 0 of the content-rendering roadmap (see `docs/REMOTION.md`). Foundation
crate: `crates/augmentagent-renderer-client`.

## Layout

```
sidecars/renderer/
  server.mjs        # Unix-socket NDJSON server, ops: ping, render
  src/index.ts      # Remotion entrypoint (registerRoot)
  src/Root.tsx      # registers the ShortCard composition
  src/ShortCard.tsx # the 1080x1920 branded title/body card
  package.json      # pinned Remotion 4.0.462 deps
  tsconfig.json
  setup.sh          # npm install + `remotion browser ensure`
```

One systemd unit in `systemd/`:

- `augmentagent-renderer.service` — runs `server.mjs`; no Xvfb/Chromium
  dependency (Remotion manages its own headless Chrome Headless Shell).

## Setup (one-time per host)

```bash
# 1. node_modules + Chrome Headless Shell for Remotion (~150 MB)
./sidecars/renderer/setup.sh

# 2. install + start the systemd unit
mkdir -p ~/.config/systemd/user
cp systemd/augmentagent-renderer.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now augmentagent-renderer.service

# 3. verify it's active
systemctl --user is-active augmentagent-renderer
```

## Composition

`ShortCard` — a clean dark branded card. inputProps:

| field         | type     | default                                   |
|---------------|----------|-------------------------------------------|
| `title`       | string   | `"AugmentAgent"`                           |
| `body`        | string   | `"A branded vertical card …"`             |
| `accent`      | string?  | `"#5B8DEF"` (hex; falls back if empty)    |
| `durationSec` | number   | `20` (drives `durationInFrames` @ 30 fps) |

The React is deterministic: spring entrance + a progress bar, all pure
functions of `frame`. No network or fonts beyond the system sans stack, so
renders are reproducible.

## Wire protocol

NDJSON over a Unix stream socket. Request/response envelope is identical to
the browser sidecar — see `server.mjs` header and
`crates/augmentagent-renderer-client/src/lib.rs`.

- `ping` → `{"pong": true, "ts": <epoch_s>}`
- `render` → params `{props, out_path, codec?}` → result
  `{path, bytes, duration_ms}`

Typed error kinds: `BadProps`, `RenderFailed`, `BundleFailed`, `Timeout`,
`Internal`.

## Operational notes

- The Remotion bundle is built **once** (eagerly at startup, lazily retried
  on the first `render` if the eager build failed) and the resulting
  `serveUrl` is cached for the life of the process. Subsequent renders pay
  only `selectComposition` + `renderMedia`.
- Renders are **serialized** in-process via a promise chain — Chromium frame
  extraction is CPU/RAM heavy and this box also runs the browser sidecar.
  `ping` is never blocked by an in-flight render.
- Remotion's Chrome Headless Shell is **separate** from the browser sidecar's
  Playwright/system Chromium. `npx remotion browser ensure` downloads it; the
  shared libs it needs are already present on this box (the browser sidecar
  established them).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `BundleFailed` on every render | check `~/.local/state/augmentagent/renderer.stderr.log`; usually a TS/JSX error in `src/` |
| `RenderFailed: ... Headless Shell` | re-run `sidecars/renderer/setup.sh` (Chrome Headless Shell missing) |
| `socket not present` | `systemctl --user status augmentagent-renderer` — sidecar didn't start; check logs at `~/.local/state/augmentagent/renderer.stderr.log` |
| `npx: command not found` after pulling | re-run `sidecars/renderer/setup.sh` |
