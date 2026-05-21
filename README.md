# AugmentAgent

## Grocery (new)

Order groceries from Discord. v1 ships the Giant Food Stores provider
(ported from DSado88/Grocery's PRISM API client) behind a pluggable
provider interface.

1. Fill in `.env` — `GIANT_EMAIL`, `GIANT_PASSWORD`, `GIANT_STORE_ID`,
   and optionally `GROCERY_PROXY` (Cloudflare WARP SOCKS5 to dodge
   DataDome).
2. One-time setup:
       npm run grocery:install      # sidecar deps + Playwright chromium
       npm run grocery:build
       npm run grocery:bootstrap    # interactive OTP login
3. Run the sidecar (PM2 handles it in prod):
       npm run grocery:sidecar
4. Trigger an order via Discord ("order groceries for this week") or:
       npm run grocery:order

Knowledge graph lives under `wiki/groceries/` — schema in
`schema/wiki-groceries.md`, agent behavior in `skills/grocery/SKILL.md`.
The agent stops at a Discord approval card; the user finishes checkout
in the Giant web app.
