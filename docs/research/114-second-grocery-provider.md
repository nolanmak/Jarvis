# Spike #114 — second grocery provider (Wegmans)

**Status:** scaffold landed; live-capture half is user-blocked on a Linux-only
deploy (no macOS counterpart, no phone CA installed).
**Confidence:** medium — scaffold is concrete and type-checks against the
existing `GroceryProvider` interface unchanged. Endpoints are TBD until a
real intercept capture lands.

## Problem

Grocery v1 ships **Giant Food Stores** only
(`sidecars/grocery/src/providers/giant/`, ported from `DSado88/Grocery`'s
PRISM client). The pluggable provider interface in
`sidecars/grocery/src/provider.ts` exists but is unused — adding a second
provider is the natural validation of both the abstraction itself and the
`/intercept` tool from #160.

## Approach

Recommend **Wegmans** as provider #2:

- Web SPA at `shop.wegmans.com` — no app required, intercept-friendly.
- Backend appears to be Instacart-operated (similar JSON shapes to
  Giant's Ahold Delhaize PRISM), so the abstraction is likely to hold.
- No public reverse-engineering exists yet — clean spike target.

Instacart proper is a fallback if Wegmans hits unsolvable anti-bot
challenges; broader coverage but heavier DataDome posture.

## What this PR ships

Half the spike — the **scaffold** that does not require a live session:

1. `sidecars/grocery/src/providers/wegmans/browser.ts` — Playwright
   lifecycle that mirrors `giant/browser.ts` (persistent Chrome profile
   under `~/.augmentagent/grocery/wegmans-chrome-profile`, optional
   `GROCERY_PROXY` SOCKS5 support, same stealth init script).
2. `sidecars/grocery/src/providers/wegmans/api.ts` — every endpoint
   function (`checkSession`, `login`, `requestOtp`, `verifyOtp`,
   `searchProducts`, `searchBatch`, `getProductsByIds`, `getCart`,
   `addItem`, `addItemsBatch`) is signed against the `provider.ts` types
   exactly as Giant's versions, but throws
   `"Wegmans provider is a scaffold — endpoint not yet implemented..."`
   until a real capture lands.
3. `sidecars/grocery/src/providers/wegmans/index.ts` — `WegmansProvider`
   class implementing `GroceryProvider` with the same OTP/auto-reauth
   pattern as `GiantProvider`. Note: no `withReauth` wrapper yet — the
   silent-relogin loop will be added once we know whether Wegmans uses
   `SESSION_EXPIRED`-style 409s or some other signal.
4. `sidecars/grocery/src/index.ts` — `GROCERY_PROVIDER=wegmans` now
   dispatches to `WegmansProvider`. Default remains `giant`; Giant
   behavior is byte-for-byte unchanged.
5. `wiki/groceries/providers/wegmans.md` — section headers (Auth flow /
   Search / Cart / Checkout) ready for the user to fill in post-capture.

**Validation finding so far:** the `GroceryProvider` interface in
`sidecars/grocery/src/provider.ts` accepted Wegmans without modification.
`userId` / `storeId` were already typed `string | number`, which is what
will be needed if Wegmans uses UUID-style IDs (TBD). The OTP type
(`OtpChannel`) is generic enough that whatever channel taxonomy Wegmans
uses can be mapped in. **No interface changes required as of this PR.**

## What requires live capture (user-blocked)

Per `CLAUDE.md`, AugmentAgent runs **Linux-only** with no macOS counterpart
and no mobile-CA-trust workflow in place. The remainder of the spike must
be driven by the user from a phone or Mac. Steps:

1. **Install the intercept CA on phone**

   ```bash
   # on the box:
   cd ~/claude_intercept && node src/cli.js cert
   # follow the printed instructions to install + trust the CA on iOS/Android
   ```

2. **Point the device proxy at the intercept on this box**

   ```bash
   cd ~/claude_intercept && node src/cli.js start
   # then on iOS: Settings → Wi-Fi → (network) → HTTP Proxy → Manual
   #   server = this box's LAN IP, port = whatever intercept printed (typically 8080)
   ```

3. **Capture a representative Wegmans session**

   - Open the Wegmans iOS or Android app (or visit `shop.wegmans.com` in
     mobile Safari/Chrome — both flow through the same intercept).
   - Log out, then log back in (capture the auth + OTP path).
   - Pick a store (capture store selection / `store_id` discovery).
   - Search for 3–5 different items.
   - Add 2–3 items to cart, change quantity on one, remove one.
   - Open the cart view and the checkout summary screen (stop before
     placing the order — v1 mirrors Giant in NOT automating checkout).

4. **Export the captured surface**

   ```bash
   cd ~/claude_intercept && node src/cli.js export --mode api-docs --host wegmans.com
   # and likely also
   node src/cli.js export --mode api-docs --host shop.wegmans.com
   node src/cli.js export --mode api-docs --host api.wegmans.com   # if it exists
   ```

5. **Paste the export into `wiki/groceries/providers/wegmans.md`** under
   the section headers already stubbed in this PR. Fill out:
   - Auth flow (login endpoint, OTP delivery channels, session-cookie
     names, CSRF token handling)
   - Search (path, query parameters, response shape — map to `Product`)
   - Cart (view / add / remove / quantity-update paths and shapes — map
     to `CartDetail` and `CartItem`)
   - Checkout (document only — v1 will NOT automate this)

6. **Fill in `sidecars/grocery/src/providers/wegmans/api.ts`** by
   replacing each `throw new Error(NOT_IMPLEMENTED)` with the
   `page.evaluate(fetch ...)` pattern that Giant uses. Run
   `npm run grocery:build` and `GROCERY_PROVIDER=wegmans npm run
   grocery:order` to validate end-to-end.

## Acceptance criteria

Spike is **done** when:

- [ ] `GROCERY_PROVIDER=wegmans npm run grocery:order` runs the same
  Discord approval-card flow as Giant.
- [ ] The provider abstraction needed zero changes during full
  implementation — OR any changes needed are documented and justified
  (the real finding to capture is which case it was).
- [ ] One real Wegmans order has gone through end-to-end: approval card
  → user-completed checkout in the Wegmans web app.
- [ ] `wiki/groceries/providers/wegmans.md` documents the API surface
  for future-Nolan / future-Claude.

This PR satisfies the first half: the scaffold is in tree, types align,
the selector works, and the doc trail is open.

## Risks

- **DataDome / anti-bot.** Giant required `GROCERY_PROXY=socks5://...`
  (WARP) for stable access. Wegmans' Instacart-backed surface likely
  has comparable protection — the scaffold already wires
  `GROCERY_PROXY` through unchanged, so this is one env-var change.
- **Mobile-app-only endpoints.** If the iOS app uses native/Protobuf
  endpoints that the web SPA doesn't expose, the existing
  `page.evaluate(fetch)` pattern won't work as-is and we'd need a
  separate HTTP client path. Worth checking via intercept before
  committing to the SPA approach.
- **OTP scaffolding mismatch.** This scaffold assumes Wegmans uses a
  single-shot OTP-by-channel pattern (like Giant). If Wegmans does
  something different (push-notification confirm, TOTP, magic link),
  the `LoginResult` shape may need a new variant — that would be a
  real finding for the abstraction.
- **Account lockouts.** Spike captures should be run against a
  throwaway Wegmans Shoppers Club account if at all possible.

## References

- Issue #114 — Spike: reverse-engineer a second grocery provider
- #160 — intercept tool (dependency; shipped in `src/interceptTool.ts`)
- `sidecars/grocery/` — current Giant implementation
- `schema/wiki-groceries.md` — KG schema
- `skills/grocery/SKILL.md` — agent behavior
- This PR — scaffold only; live capture pending
