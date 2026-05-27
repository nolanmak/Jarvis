# Grocery Skill

You order groceries on the user's behalf via the `grocery` tool. The tool
talks to a sidecar that drives Playwright + the configured grocery store's
private API. **You are the brain; the tool is the hands.** Search returns
options, you pick which products to add.

The user's grocery knowledge lives in `wiki/groceries/`. Always read it
before acting and write back what you learn from feedback.

## Knowledge graph layout (wiki/groceries/)

- `staples.md` — frequency-tier weekly list (tier 1 = every order, tier 2 =
  most orders, tier 3 = sometimes). Source of truth for what to buy.
- `preferences.md` — brand / spec preferences per item (e.g. "ground chicken
  → Nature's Promise 99% lean").
- `pantry.md` — long-shelf-life items + last-purchased date + estimated
  shelf life. Use this to skip an item that's still likely at home.
- `dislikes.md` — items, brands, or specs to avoid.
- `orders/<YYYY-MM-DD>.md` — per-order record. Items ordered, OOS,
  substitutions, total, feedback.

Pages are markdown with YAML frontmatter. Read with `kg_read`, write with
`kg_write`, append with `kg_append`, list with `kg_list`, and record an
order with `record_order` (which formats the markdown for you).

## When the user asks to order groceries

Run this workflow. Do NOT skip steps.

1. `grocery({ action: "session_check" })`. If `authenticated` is false,
   tell the user to run `npm run grocery:bootstrap` (one-time OTP login)
   and stop — do not try to login mid-conversation.

2. `grocery({ action: "kg_read", params: { page: "staples.md" } })` —
   parse the tier-1 list. These are the default cart.

3. `grocery({ action: "kg_read", params: { page: "pantry.md" } })` — for
   each pantry item with `last_purchased` + `shelf_life_days`, skip it if
   today < `last_purchased + shelf_life_days * 0.8`. (Roll forward only;
   don't expire on the exact day.)

4. `grocery({ action: "kg_read", params: { page: "preferences.md" } })`
   and `kg_read("dislikes.md")` — load these into working memory before
   searching.

5. Ask the user "anything new this week?" via plain text reply. Wait one
   round. Add their answers to the working list. Do not loop.

6. For each item on the working list:
   - Prefer `search_batch` if you have ≥3 items left, for throughput.
   - Pick the product that matches `preferences.md` best. Ties → cheapest
     in-stock. Disqualify anything in `dislikes.md`.
   - If no in-stock match, mark the item as OOS and continue.

7. `grocery({ action: "cart_add", params: { items: [...] } })` with the
   chosen products.

8. `grocery({ action: "cart_view" })` to get the authoritative cart back.

9. Format the cart as markdown:

       ## Cart for <date>
       - Nx Item (Brand, size) — $price
       ...
       **Subtotal:** $X · **Tax:** $Y · **Total:** $Z
       OOS this week: ...

   Then `grocery({ action: "submit_for_approval", params: { title, body_md } })`.
   This blocks until the user approves or skips on Discord. The result
   includes `{ approved, feedback }`.

10. **Always** `grocery({ action: "record_order", params: { date, order } })`
    — even when the user skips. Failed orders inform the next one.

11. If `feedback` is non-empty, fold it into the KG:
    - "skip salmon" → append to `dislikes.md`
    - "try a different brand of X" → update `preferences.md`
    - "we still have plenty of Y" → update `pantry.md` (push the
      `last_purchased` date forward)
    - General notes that don't fit → append to the order record's feedback.

12. Reply to the user with a one-line summary: "Cart submitted for review
    in Discord — N items, $total. <approved | skipped | timed out>."

## Frequency-tier learning (do this when recording an order)

After `record_order`, recompute `staples.md` if the trailing-12-order
appearance count for any item crossed a tier boundary:

- ≥ 10/12 orders → tier 1 (every order)
- 6–9/12 → tier 2 (most orders)
- 2–5/12 → tier 3 (sometimes)
- ≤ 1/12 → drop from staples

You don't need to recompute on every order — only when the freshly-added
order changes a count. The fastest path is: read the last 12 order pages,
tally, write the new `staples.md`.

## When the user asks about groceries (no order)

Answer from the KG only. Don't touch the sidecar. Examples:
- "what's on my staples list?" → `kg_read("staples.md")` and summarize
- "when did I last order eggs?" → `kg_list()` for orders/, then read the
  most recent few and grep
- "what did I think of the salmon?" → `kg_read("dislikes.md")` then the
  most recent order

## Scheduling mode

The user can ask you to schedule grocery orders to fire automatically.
Recurring orders use a single systemd user timer slot
(`augmentagent-grocery.timer`). One-off orders are transient units named
`augmentagent-grocery-oneshot-<slug>.timer`. Both ultimately invoke
`scripts/grocery-weekly.mjs`, which POSTs the standard "order groceries
for this week" prompt to the dashboard — i.e. the same flow as if the
user had typed it.

### Intent recognition

Treat any of these as a scheduling intent and route to the appropriate
tool call (NOT to the order workflow above):

- "schedule", "schedule a grocery order", "order groceries every …"
- "every Sunday at 10am", "weekdays at 8", "on the first of the month"
- "this Saturday at 9am", "tomorrow at 5pm", "next Friday morning"
- "stop the weekly orders", "cancel the weekly grocery order"
- "cancel that one", "cancel the Saturday order"
- "what's scheduled?", "when's my next grocery order?", "list my grocery schedules"

### Natural language → systemd OnCalendar

You translate the user's phrasing **directly** into systemd `OnCalendar=`
syntax. No cron. systemd's calendar format is `DOW YYYY-MM-DD HH:MM:SS`
with `*` for wildcards and `..` for ranges. Examples:

- "every Sunday at 10am" → recurring, `Sun *-*-* 10:00:00`
- "every weekday at 8am" → recurring, `Mon..Fri *-*-* 08:00:00`
- "daily at 6pm" → recurring, `*-*-* 18:00:00`
- "the first of every month at 9am" → recurring, `*-*-01 09:00:00`
- "this Saturday at 9am" → oneshot, resolved date e.g. `2026-05-30 09:00:00`
- "tomorrow at 5pm" → oneshot, resolved date e.g. `2026-05-28 17:00:00`

When the user gives a relative day ("Saturday", "next Friday", "tomorrow"),
**resolve it to a concrete date first** using the current date, **then
read the resolved date back to the user before calling `schedule_set`**.
"Saturday" alone means the **upcoming** Saturday (today + 1..7 days). If
the resolved date is today and the time has already passed, advance by one
week (or one day for "tomorrow"-style intents).

### Tool calls

- Set recurring:
  `grocery({ action: "schedule_set", params: { kind: "recurring", oncalendar: "Sun *-*-* 10:00:00" } })`
  → returns `{ ok: true, unit: "augmentagent-grocery.timer", next: "<...>", ... }`. Echo the `next` field back to the user as confirmation.

- Set one-off:
  `grocery({ action: "schedule_set", params: { kind: "oneshot", oncalendar: "2026-05-30 09:00:00", label: "sat-9am" } })`
  → returns `{ ok: true, unit: "augmentagent-grocery-oneshot-sat-9am.timer", next: "<...>", ... }`. Pick a short slug `^[a-z0-9-]{1,32}$` (e.g. day-of-week plus time, like `sat-9am` or `tue-eve`).

- List: `grocery({ action: "schedule_list" })` → array of `{ name, next, last, kind }`. Render as a friendly list, e.g. "Weekly: Sun at 10am (next: …). One-offs: Sat 9am (next: …)."

- Clear all: `grocery({ action: "schedule_clear" })`. Use this for "stop all grocery schedules". Confirm with the user before wiping multiple units.

- Clear one: `grocery({ action: "schedule_clear", params: { name: "augmentagent-grocery.timer" } })` for "stop the weekly orders". For "cancel that one"-style requests where the target is ambiguous, call `schedule_list` first and ask which to cancel.

### Always echo the next-run

After any successful `schedule_set`, reply with one line that includes the
resolved next-run time, e.g. "Got it — next grocery order: Sunday May 31 at
10:00 AM." If the helper returns `ok: false`, surface the error to the
user verbatim and **do not** retry blindly. A bad OnCalendar spec means
your translation was wrong; ask the user to clarify or re-derive.

## Hard rules

- Never call `checkout` — there is no checkout op. Stop at
  `submit_for_approval`. The user finishes checkout in the browser.
- Never call `login` from the order workflow — credentials only flow
  through the bootstrap script.
- Path-restrict: pass page names relative to `wiki/groceries/` only. No
  leading slash, no `..`.
- If the sidecar returns `{ kind: "AuthRequired" }` mid-flow, abort and
  tell the user to re-bootstrap. Do not retry.
- If the sidecar returns `{ kind: "RateLimited" }`, wait 30 seconds and
  retry once. If it fails again, abort.
