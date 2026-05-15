# Remotion renderer — design + phased roadmap

AugmentAgent renders short vertical videos (1080×1920) for outbound social
content. This document covers the design and the phased rollout. **Phase 0
(this PR) is the only thing built so far** — a manually-triggerable,
end-to-end "render an mp4 from JSON props" path. Everything in Phases 1+ is
roadmap, not code.

## Why Remotion (and what was considered)

| Option | License | Verdict |
|--------|---------|---------|
| **[Remotion](https://www.remotion.dev/)** | Free for individuals / small teams; a company license only triggers above a revenue/headcount threshold this solo-dev deployment is nowhere near ($0 here) | **Chosen.** Mature renderer API (`bundle()` → `selectComposition()` → `renderMedia()`), JSON `inputProps` in / mp4 out, bundled ffmpeg in `@remotion/compositor-linux-x64-gnu`, auto-downloads its own Chrome Headless Shell. React authoring matches the existing Node dashboard stack. |
| **[Revideo](https://re.video/)** (Motion-Canvas fork) | MIT | Considered as the lighter, fully-permissive alternative. Genuinely simpler runtime, but a smaller ecosystem and a canvas/generator authoring model that's further from the team's React familiarity. Kept on the table as the fallback if Remotion's licensing posture ever changes for this deployment; for Phase 0 the Remotion renderer API was the faster, lower-risk path. |

The box already runs a Chromium sidecar, so the Linux shared libraries
headless Chrome needs are present — Remotion's `npx remotion browser ensure`
only has to fetch the Chrome Headless Shell binary itself (~150 MB), separate
from the browser sidecar's Playwright/system Chromium.

## Architecture — mirrors the browser sidecar

The renderer is a **sidecar**, deliberately built to the same pattern as
`sidecars/browser/` so there's one IPC convention in the codebase:

```
augmentagent render  (CLI)
        │
        ▼
crates/augmentagent-renderer-client   (Rust, tokio, request_id-demux)
        │  NDJSON over Unix stream socket
        │  ${XDG_RUNTIME_DIR}/augmentagent/renderer.sock
        ▼
sidecars/renderer/server.mjs          (Node, long-running)
        │  bundle() once → serveUrl cached
        ▼
@remotion/renderer  selectComposition + renderMedia
        │
        ▼
   /tmp/whatever.mp4   (1080×1920 h264)
```

### Wire protocol (identical envelope to the browser sidecar)

```
Request : {"request_id": "<uuid>", "op": "<name>", "params": {...},
           "timeout_ms": 300000}
Success : {"request_id": "...", "ok": true,  "result": {...},
           "elapsed_ms": 8123}
Failure : {"request_id": "...", "ok": false,
           "error": {"kind": "<Kind>", "message": "..."},
           "elapsed_ms": 300000}
```

Ops:

- `ping` → `{"pong": true, "ts": <epoch_s>}` — never blocked by a render.
- `render` → params `{props, out_path, codec?}` → result
  `{path, bytes, duration_ms}`.

Typed error kinds: `BadProps`, `RenderFailed`, `BundleFailed`, `Timeout`,
`Internal`. The Rust client (`RendererError`) exposes `is_bad_props()`,
`is_render_failed()`, `is_bundle_failed()`, `is_timeout()` for branching
without parsing the message — same convenience-predicate pattern as
`BrowserError`.

### The composition

`ShortCard` (`sidecars/renderer/src/ShortCard.tsx`): a clean dark branded
title/body card, 1080×1920, 30 fps, default 20 s. inputProps
`{title, body, accent?, durationSec}`. The React is **deterministic** —
spring entrance + progress bar, all pure functions of `frame`, no network or
external fonts — so renders are reproducible. `durationSec` drives
`durationInFrames` via Remotion's `calculateMetadata`, so the Rust client
controls clip length without the sidecar re-bundling.

### Performance notes

- The Remotion bundle is built **once** (eagerly at startup, lazily retried
  on first render if the eager build failed); `serveUrl` is cached for the
  process lifetime. Subsequent renders pay only `selectComposition` +
  `renderMedia`.
- Renders are **serialized** in-process via a promise chain. Chromium frame
  extraction is CPU/RAM heavy and this box also runs the browser sidecar;
  one render at a time keeps the box responsive. `ping` is not serialized.

## Phased roadmap

### Phase 0 — render path (THIS PR) ✅

- `sidecars/renderer/` Node service: Remotion project + NDJSON Unix-socket
  server, bundle caching, typed errors.
- `crates/augmentagent-renderer-client` workspace crate (tokio, serde,
  request_id-demux), unit-tested.
- `systemd/augmentagent-renderer.service` user unit (Restart=on-failure,
  logs to `~/.local/state/augmentagent/renderer.{stdout,stderr}.log`).
- `augmentagent render --props … --out … --codec …` CLI subcommand.

**Scope boundary — intentionally NOT in Phase 0** (deferred so the render
primitive lands reviewable and isolated, and so the ToS/rate-safety surface
gets its own focused review):

- No scheduler, no `WorkItem`/trigger, no governor wiring.
- No social posting (no platform publish, no ToS-bearing actions).
- No `ApprovalBroker` changes.
- No content-adapter fan-out.

### Phase 1 — content-adapter fan-out

Implement the stubbed types in `crates/augmentagent-content-adapter`
(`SourceDraft`, `PlatformVariant`, `MediaSpec` — today a placeholder in
`types.rs`). A `SourceDraft` (an approved long-form idea) fans out to
per-platform `PlatformVariant`s, each carrying a `MediaSpec` that maps onto
`ShortCard` inputProps. Adapter calls `RendererClient::render` to produce the
mp4 per variant.

### Phase 2 — governor `ActionKind::Post` wiring

`crates/augmentagent-channel-core/src/governor` already models
`ActionKind::Post` in the 2025 cap matrix
(`governor/limits.rs`) but nothing emits Post actions yet. Wire the content
pipeline through the `RateGovernor` with warmup ramp, jitter, and
quiet-hours so automated posting can't trip platform anti-spam. This is the
first phase that touches rate-safety and must get its own review.

### Phase 3 — ApprovalBroker media attachment

Extend `augmentagent-approval-discord` so an approval card can carry the
rendered mp4 as a Discord attachment (today `ApprovalBroker::approve` is
text-only). The operator previews the actual video before approving a post —
same human-in-the-loop gate Gmail/LinkedIn drafts already use.

### Phase 4 — outbound trigger/scheduler

Introduce an outbound `WorkItem` of `kind="content_post"` and a
trigger/scheduler so content posts can be queued and time-released
(respecting Phase 2's governor) rather than only manually `augmentagent
render`'d.

### Phase 5 — platform publish

Actually publish. **Status of the platform surface today:**

- **LinkedIn**: a DM channel crate exists
  (`crates/augmentagent-channel-linkedin`) but **feed/post publishing is
  unimplemented** — only messaging is wired.
- **Instagram / Twitter(X)**: **no crates exist.** Net-new channel work,
  each with its own auth + ToS review.

Every publish path in this phase is ToS-bearing and rate-sensitive; it is
deliberately the last phase and gated on Phases 2–4.

## Operational

Setup, systemd install, and troubleshooting live in
`sidecars/renderer/README.md` and `systemd/README.md`.
