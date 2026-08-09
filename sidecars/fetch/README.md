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
- Firecrawl uses the current `/v2/scrape` API and requests main-content
  markdown plus HTML. Bright Data uses the Unlocker `/request` API with
  `data_format: markdown` and accepts both its raw response and documented
  JSON `{ body, ... }` envelope.
- Set `FETCH_DRY_PROVIDERS=1` to skip live provider calls during testing.

## Install + run

```sh
npm run fetch:install     # installs deps + Chromium
npm run fetch:build
npm run fetch:sidecar     # starts the socket server
npm run fetch:smoke https://example.com   # standalone in-process smoke test
```

## Related

- Epic: `#141`
- Sub-issues: `#142` (skeleton + http), `#143` (render), `#144` (firecrawl),
  `#145` (bright data), `#146` (agent tool), `#147` (intercept tool — separate).
