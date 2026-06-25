# fetch sidecar

Layered URL-fetch sidecar. The agent calls `web_fetch(url)`; the sidecar
internally tries strategies in order and returns the first that yields
usable content.

## Layers

| # | Layer        | When                                              | Configuration                |
|---|--------------|---------------------------------------------------|------------------------------|
| 1 | `http`       | Always tried first. Plain HTTPS + html→markdown.  | none                         |
| 2 | `render`     | If layer 1 looks like a JS-rendered SPA shell.    | Playwright (chromium)        |
| 3 | `firecrawl`  | If layer 2 fails or returns thin content.         | `FIRECRAWL_API_KEY`          |
| 4 | `brightdata` | Final fallback for anti-bot / captcha sites.      | `BRIGHTDATA_API_KEY` + `BRIGHTDATA_ZONE` |

Layers without configured keys are skipped automatically. The local layers
(http + render) have no external dependencies beyond Playwright/Chromium.

## Quality heuristic

After each layer succeeds, the sidecar checks the resulting markdown:

- If shorter than `min_quality_chars` (default 400), escalate.
- If raw HTML matches a known SPA-shell signal (e.g. empty `<div id="root">`,
  `__NEXT_DATA__` script), escalate.

If every layer is exhausted, the **best** (longest) output is returned with
the failing `attempts` log so the agent can decide what to do.

## Wire protocol

NDJSON over a Unix socket — same shape as `sidecars/grocery`.

Request frame:
```json
{ "request_id": "uuid", "op": "fetch", "params": { "url": "https://..." }, "timeout_ms": 90000 }
```

Response frame:
```json
{
  "request_id": "uuid",
  "ok": true,
  "result": {
    "url": "...",
    "final_url": "...",
    "status": 200,
    "title": "...",
    "markdown": "...",
    "layer_used": "render",
    "attempts": [{ "layer": "http", "ok": false, "reason": "..." }, ...],
    "elapsed_ms": 1234
  }
}
```

## Cost notes

- Layers 1-2 are free.
- Firecrawl is ~$0.001/scrape (scales with rendering complexity).
- Bright Data Unlocker is ~$1/1000 requests for the basic Unlocker. Materially
  more expensive than Firecrawl; only invoked as the last fallback.
- Set `FETCH_DRY_PROVIDERS=1` to skip live provider calls during testing.

## Install + run

```sh
npm run fetch:install     # installs deps + Chromium
npm run fetch:build
npm run fetch:sidecar     # starts the socket server
npm run fetch:smoke https://example.com   # standalone in-process smoke test
```

## Related

- Epic: [#107](https://github.com/nolanmak/MyAgentAssistant/issues/107)
  — "[epic] Layered web-fetch tool (HTTP → headless → Firecrawl → Bright Data)".
- Sub-issues: skeleton + http, render, firecrawl, bright data, agent tool, and
  the (separate) intercept tool. These were the `#142`–`#147` series in the
  archived `nolanmak/AugmentAgent` repo; those numbers do **not** map to the
  same issues in `nolanmak/MyAgentAssistant`, so the breakdown lives in epic
  #107's checklist rather than as standalone links here.
