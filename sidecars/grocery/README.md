# Grocery sidecar

Long-running Node + Playwright process that handles per-provider grocery
ordering. Giant Food Stores (PRISM API) was the first provider; Wegmans is
also supported. Select with `GROCERY_PROVIDER` (`giant` | `wegmans`).

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
| schedule_set   | { kind: "recurring" \| "oneshot", oncalendar, label? } |
| schedule_list  | -                                               |
| schedule_clear | { name? }                                       |

The `schedule_*` ops manage systemd user **order timers** (recurring or
one-shot grocery orders) by shelling out to `scripts/grocery-schedule.mjs`.
For `schedule_set`, `oncalendar` is a systemd `OnCalendar` spec and `label`
(one-shot only) must match `^[a-z0-9-]{1,32}$`. These timers fire scheduled
orders — they do **not** supervise the sidecar process itself.

There is intentionally **no `checkout` op**. v1 stops after building the
cart: the user finishes checkout themselves in the Giant web app after the
Discord approval card lands. Adding automated checkout (slot selection,
payment confirm) is tracked for later.

## Env vars

| var                       | purpose                                         |
| ------------------------- | ----------------------------------------------- |
| GROCERY_PROVIDER          | provider id, `giant` (default) or `wegmans`     |
| GROCERY_SOCKET            | override socket path                            |
| GROCERY_HEADLESS          | run Chromium headless (giant + wegmans)         |
| GIANT_EMAIL               | Giant login email                               |
| GIANT_PASSWORD            | Giant login password                            |
| GIANT_STORE_ID            | Giant store id (numeric, e.g. 0356)             |
| GIANT_STORE_BASE_URL      | default `https://giantfoodstores.com`           |
| WEGMANS_EMAIL             | Wegmans login email                             |
| WEGMANS_PASSWORD          | Wegmans login password                          |
| WEGMANS_STORE_ID          | Wegmans store id (required for wegmans)          |
| WEGMANS_STORE_BASE_URL    | default `https://shop.wegmans.com`              |
| GROCERY_CHROME_PROFILE    | persistent Chrome profile dir                   |
| GROCERY_PROXY             | optional SOCKS5/HTTP proxy (e.g. WARP)          |

## One-time setup

    bash setup.sh

## Run

    npm run build && npm start

From the repo root you can also use the wrapper scripts:
`npm run grocery:build` then `npm run grocery:sidecar`.

> **Note:** the grocery sidecar is **not** registered in the root
> `ecosystem.config.js` (which only supervises `augmentagent` and
> `fetch-sidecar`), so PM2 does not currently manage it. Scheduled orders are
> driven separately by the `schedule_*` ops, which create systemd user order
> timers via `scripts/grocery-schedule.mjs`.
