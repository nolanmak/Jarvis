# Grocery sidecar

Long-running Node + Playwright process that handles per-provider grocery
ordering (Giant Food Stores PRISM API is the first provider).

Speaks the same NDJSON-over-Unix-socket protocol as `sidecars/browser`:

    Request : {"request_id": "<uuid>", "op": "<name>", "params": {...},
               "timeout_ms": 30000}
    Success : {"request_id": "...", "ok": true,  "result": {...},
               "elapsed_ms": 412}
    Failure : {"request_id": "...", "ok": false, "error": {
                 "kind": "AuthRequired" | "OTPRequired" | "OutOfStock"
                       | "NotFound" | "RateLimited" | "Timeout"
                       | "CartError" | "Internal",
                 "message": "...", "diagnostic": "..." }, "elapsed_ms": 30000}

Socket path: `${XDG_RUNTIME_DIR}/augmentagent/grocery.sock`
(falls back to `/tmp/augmentagent/grocery.sock` on macOS).

## Ops

| op             | params                                          |
| -------------- | ----------------------------------------------- |
| ping           | -                                               |
| session_check  | -                                               |
| login          | { email?, password? }  (omit to read from env)  |
| verify_otp     | { code, channel? }                              |
| search         | { query, limit? }                               |
| search_batch   | { queries: string[], limit_per_query? }         |
| products_by_id | { prodIds: number[] }                           |
| cart_view      | -                                               |
| cart_add       | { items: [{ productId, quantity }] }            |
| cart_remove    | { productId }                                   |

There is intentionally **no `checkout` op**. v1 stops after building the
cart: the user finishes checkout themselves in the Giant web app after the
Discord approval card lands. Adding automated checkout (slot selection,
payment confirm) is tracked for later.

## Env vars

| var                       | purpose                                         |
| ------------------------- | ----------------------------------------------- |
| GROCERY_PROVIDER          | provider id, default `giant`                    |
| GROCERY_SOCKET            | override socket path                            |
| GIANT_EMAIL               | Giant login email                               |
| GIANT_PASSWORD            | Giant login password                            |
| GIANT_STORE_ID            | Giant store id (numeric, e.g. 0356)             |
| GIANT_STORE_BASE_URL      | default `https://giantfoodstores.com`           |
| GROCERY_CHROME_PROFILE    | persistent Chrome profile dir                   |
| GROCERY_PROXY             | optional SOCKS5/HTTP proxy (e.g. WARP)          |

## One-time setup

    bash setup.sh

## Run

    npm run build && npm start

PM2 manages it in production via `ecosystem.config.js` (see project root).
