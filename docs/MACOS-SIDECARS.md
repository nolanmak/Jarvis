# macOS sidecar services

The browser stack, renderer, fetch worker, and WhatsApp sidecar run as
per-user LaunchAgents. Install them from a logged-in macOS user session.
Node.js 22+, Python 3, and the build tools named below must be available.
The socket directory is `/tmp/augmentagent-<uid>/` with mode `0700`; each
socket is owner-only. This short path works even when the home directory has
spaces, non-ASCII names, or exceeds the macOS Unix socket path limit.

| Sidecar | Build and install | Status and logs | Remove job |
| --- | --- | --- | --- |
| Browser stack | `sidecars/browser/setup.sh`; `augmentagent install browser-sidecar` | `augmentagent browser status`; `augmentagent logs --unit augmentagent-browser-sidecar.service` | `augmentagent uninstall browser-sidecar` |
| Renderer | `sidecars/renderer/setup.sh`; `python3 scripts/install-sidecar.py renderer` | `augmentagent service --unit augmentagent-renderer.service status`; `augmentagent logs --unit augmentagent-renderer.service` | `bash scripts/uninstall-sidecar.sh renderer` |
| Fetch | `sidecars/fetch/setup.sh`; `python3 scripts/install-sidecar.py fetch` | `augmentagent service --unit augmentagent-fetch.service status`; `augmentagent logs --unit augmentagent-fetch.service` | `bash scripts/uninstall-sidecar.sh fetch` |
| WhatsApp | `sidecars/wa-sidecar/setup.sh`; `python3 scripts/install-sidecar.py wa-sidecar` | `augmentagent service --unit augmentagent-wa-sidecar.service status`; `augmentagent logs --unit augmentagent-wa-sidecar.service` | `bash scripts/uninstall-sidecar.sh wa-sidecar` |

The renderer build installs its pinned Remotion dependencies and Chrome
Headless Shell. Fetch uses its committed npm lock and installs Playwright
Chromium. WhatsApp uses the Go toolchain from `go.mod`; its `setup.sh`
verifies modules and builds the exact binary used by launchd. Browser setup
uses its Python venv and Playwright browser. The installer refuses to load a
job when its built dependency is missing. Installation replaces a loaded job
transactionally and restores the previous plist if launchd rejects the new
one. Uninstall stops only the named job and retains browser profiles,
WhatsApp pairing state, logs, and other sidecar data.

Fetch can run the local HTTP/render layers without paid provider keys. For
Firecrawl or Bright Data, put only `FIRECRAWL_API_KEY`, `BRIGHTDATA_API_KEY`,
and `BRIGHTDATA_ZONE` in `~/.config/augmentagent/fetch.env` as plain
`KEY=value` lines. Keep the containing directory mode `0700` and the file
mode `0600`. The launcher rejects a public, non-regular, symlinked, duplicate,
or unknown credential entry. It reads secrets into the fetch process
environment; no key appears in the plist or process arguments. The service
does not load a checkout-local `.env` file.

`scripts/check-for-updates.sh` rebuilds a changed sidecar and restarts it only
when its user job is installed. It handles one sidecar at a time, preserves
sessions, and withholds the build stamp when a dependency build or required
restart fails. Use the status and logs commands above after an update.

Automated Mac and Linux checks use private temporary directories, synthetic
pages and media props, and offline WhatsApp protocol fixtures. They do not
prove a signed-in browser consent flow or real WhatsApp pairing. Record those
separately with test accounts before closing [#1256](https://github.com/nolanmak/Jarvis/issues/1256).
