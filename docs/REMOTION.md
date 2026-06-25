# Remotion renderer — design + phased roadmap

AugmentAgent renders short vertical videos (1080×1920) for outbound social
content. This document covers the design and the phased rollout. **Phase 0
shipped first** — a manually-triggerable, end-to-end "render an mp4 from JSON
props" path. Since then much of the surrounding pipeline has landed too: the
content-adapter fan-out (Phase 1), governor `ActionKind::Post` wiring (Phase
2), and the Instagram / Twitter(X) / LinkedIn posting channels (Phase 5) are
all built. What remains genuinely unbuilt is the renderer→adapter wiring (no
crate calls `RendererClient::render` for a `MediaSpec` yet) and end-to-end
video publish. See the [phase status table](#phase-status) below.

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
           "timeout_ms": 120000}
Success : {"request_id": "...", "ok": true,  "result": {...},
           "elapsed_ms": 8123}
Failure : {"request_id": "...", "ok": false,
           "error": {"kind": "<Kind>", "message": "..."},
           "elapsed_ms": 120000}
```

`timeout_ms` is per-op, not a single fixed envelope value: the sidecar
defaults to `120000` when the field is omitted (`server.mjs`), while the Rust
client sends `5000` for `ping` and `DEFAULT_RENDER_TIMEOUT_MS = 300000` for
`render`.

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

### Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| 0 | Render path (sidecar + client + CLI) | ✅ Done |
| 1 | content-adapter fan-out (`SourceDraft` → `PlatformVariant`/`MediaSpec`) | ✅ Done (#53, #172, #241); renderer→adapter wiring still unbuilt |
| 2 | governor `ActionKind::Post` wiring | ✅ Done (#58) |
| 3 | `ApprovalBroker` media attachment | ◻ Planned |
| 4 | Outbound trigger/scheduler | ◻ Planned |
| 5 | Platform publish (LinkedIn / Instagram / Twitter channels) | ◐ Partial — channels built; video publish wiring unbuilt |

### Phase 0 — render path ✅

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

### Phase 1 — content-adapter fan-out ✅ (#53)

`crates/augmentagent-content-adapter` is implemented. `SourceDraft`,
`PlatformVariant`, and `MediaSpec` are real types in `types.rs` (with
builders, per-platform char limits, thread/over-limit handling, and unit
tests). A `SourceDraft` (an approved long-form idea) fans out to per-platform
`PlatformVariant`s via `adapter::fan_out`, each carrying a `MediaSpec` that
maps onto `ShortCard` inputProps. The crate also ships `preview::preview_all`
/ `variant_card` preview rendering, a SocialAPI.ai cross-post fan-out
(`socialapi.rs`, #241), and a post-time publish orchestrator
(`publish.rs`, #172).

**Still unbuilt — renderer→adapter wiring.** The adapter is a pure text
transform: it emits a `MediaSpec` (sizing + alt text), it does **not** invoke
the renderer or produce an mp4 (`crates/augmentagent-content-adapter` has no
dependency on `augmentagent-renderer-client`, and `types.rs` notes the
`MediaSpec` is the *spec a downstream renderer/uploader consumes*, not
pixels). Nothing yet takes a `MediaSpec` and calls `RendererClient::render`
to produce the mp4 per variant — that wiring remains future work.

### Phase 2 — governor `ActionKind::Post` wiring ✅ (#58)

`crates/augmentagent-channel-core/src/governor` models `ActionKind::Post` in
the 2025/26 cap matrix (`governor/limits.rs`), and the engagement engine now
emits it: `engagement.rs`'s scheduled-post fire loop (`fire_one`) builds an
`ActionRequest { action: ActionKind::Post, … }` and runs it through the
`RateGovernor` permit/record envelope before publishing. This is the first
phase that touches rate-safety. Any remaining hardening (warmup-ramp tuning,
jitter, quiet-hours edge cases) builds on top of this wiring rather than
introducing the Post emitter from scratch.

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

### Phase 5 — platform publish ◐ (channels built)

The per-platform channel crates — auth + DM + posting — already exist.
**Status of the platform surface today:**

- **LinkedIn** (`crates/augmentagent-channel-linkedin`): feed posting is
  implemented in `posting.rs` (#51 / #77) via Voyager
  `contentcreation/normShares` — **text** and **single-image** posts, with
  `ShareUrn` / `Visibility` types and own-post comment polling (`own_posts.rs`,
  #58.2). Deferred to a later sub-phase: video, polls, scheduling, articles,
  and multi-image.
- **Instagram** (`crates/augmentagent-channel-instagram`): exists with auth +
  DM surfaces and a browser-driven `Composer` (`composer.rs`) for
  Feed/Carousel/Reel/Story posting (#50 / #76).
- **Twitter(X)** (`crates/augmentagent-channel-twitter`): exists with auth +
  DM surfaces and a `CreateTweetClient` (#79) that posts via the `CreateTweet`
  GraphQL op behind a hard 15/day quota preflight and a dry-run gate.

**Still unbuilt — video publish wiring.** What remains is connecting the
renderer-produced mp4 (Phase 1's `MediaSpec` → rendered clip) through these
channels' posting paths; the channels currently post text/image, not the
rendered short. Every publish path here is ToS-bearing and rate-sensitive, so
the remaining video-publish wiring stays gated on Phases 2–4.

## Operational

Setup, systemd install, and troubleshooting live in
`sidecars/renderer/README.md` and `systemd/README.md`.
