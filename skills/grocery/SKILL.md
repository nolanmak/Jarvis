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

## Setup & edit mode

Sometimes the user wants to **populate or edit** the KG, not place an order.
Route these intents through `kg_read` / `kg_write` / `kg_append` — never ask
the user to hand-edit YAML. The order workflow below assumes the four KG
files already exist with real content; this section is how they get there.

YAML you write must conform to `schema/wiki-groceries.md` exactly. After
every write, immediately `kg_read` the same page to confirm the round-trip
parses cleanly. If it doesn't, fix it before replying to the user.

Today's date for any `last_purchased` field comes from the system context
that's already provided at runtime — don't ask the user.

### Universal rules (apply to every intent below)

- **Preserve existing entries.** Always `kg_read` first, mutate in working
  memory, then `kg_write` the full file back. Never drop fields you didn't
  intend to change.
- **Confirm destructive operations before writing.** "Replace my entire
  staples list", "clear my pantry", "wipe my dislikes" — paraphrase what
  you're about to do and wait for a yes.
- **Show the YAML before writing on bulk changes** (initial setup,
  replace-all). For single-line edits (one staple added, one dislike), just
  do it and tell the user what changed.
- **Tier defaults:** tier-1 = every order, tier-2 = most orders, tier-3 =
  sometimes. If the user says "add X" without specifying frequency, default
  to tier-1.

### 1. Initial setup

Triggers: "set up my grocery list", "let's configure groceries", "I want to
set up groceries", "help me get started with groceries".

Workflow:

1. For each of `staples.md`, `preferences.md`, `pantry.md`, `dislikes.md`,
   try `kg_read`. If any file already has non-trivial content (more than
   just the `type:` line and an empty list), ask: **"You already have
   entries in `<file>`. Replace them, or merge new answers in?"** Wait for
   an answer before proceeding.
2. Walk the user through four steps, one at a time. Don't dump all four
   questions at once.

   **Step 1 — staples.** Ask: "What do you want on every order? (Things
   like eggs, bread, milk — tier-1.)" Then: "Anything you buy most weeks
   but not every week? (tier-2)" Then: "Anything occasional? (tier-3)"
   For each item, ask qty if unclear (default 1).

   **Step 2 — preferences.** "Any specific brands or specs you want for
   these items? E.g. '99% lean ground chicken', 'Nature's Promise eggs',
   'unsalted butter'." One item per line is fine.

   **Step 3 — pantry.** "What long-shelf-life staples do you already have
   at home? (rice, paper towels, OTC meds, canned goods, dry pasta — stuff
   I should skip if you bought it recently.)" For each, ask roughly when
   they last bought it (default to today if unsure) and use the shelf-life
   defaults below.

   **Step 4 — dislikes.** "Anything to always skip? Brands, food items, or
   descriptors like 'canned' or 'low-sodium'."

3. Compose all four YAML files in working memory. **Show them to the user
   as a single fenced block per file** and ask: "Look right? I'll write
   these to `wiki/groceries/` on your confirmation." Wait for yes.
4. `kg_write` each file. Then `kg_read` each one back to verify
   round-trip.
5. Reply: "Grocery KG set up — N staples (tier-1: a, tier-2: b, tier-3: c),
   M preferences, P pantry items, Q dislikes. You can now say 'order
   groceries' to place an order."

Example shape for `staples.md` after step 1:

    ---
    type: grocery-staples
    last_recomputed: null
    tiers:
      "1_every_order":
        - { item: eggs, qty: 1 }
        - { item: whole milk, qty: 1 }
      "2_most_orders":
        - { item: greek yogurt, qty: 2 }
      "3_sometimes":
        - { item: ice cream, qty: 1 }
    ---

### 2. Add to staples

Triggers: "add X to my staples", "I always want Y on the list", "put Z on
every order".

Workflow:

1. `kg_read("staples.md")`. Parse the YAML.
2. Pick the tier: if the user said "every order" / "always" / "weekly" →
   tier-1. "Most weeks" / "usually" → tier-2. "Sometimes" / "occasionally"
   → tier-3. **No qualifier → default tier-1.**
3. Check whether the item is already present in any tier. If yes, ask:
   "X is already on tier-N — move it to tier-1, or keep it where it is?"
4. Append the new entry `{ item: <name>, qty: <n> }` (default qty 1) to the
   chosen tier. Preserve all other tier entries verbatim.
5. `kg_write("staples.md", <full updated YAML>)`. Then `kg_read` to verify.
6. Reply: "Added X (qty N) to tier-K staples."

Example: user says "add 99% lean ground chicken to my staples". Read
`staples.md`, append `- { item: 99% lean ground chicken, qty: 1 }` to
`"1_every_order"`, write back, verify.

### 3. Set preference

Triggers: "I prefer brand Z for X", "for X I like the Y version", "always
get the unsalted butter".

Workflow:

1. `kg_read("preferences.md")`. Parse `items`.
2. Find the existing entry for `<item>` (case-insensitive substring match).
   - **If found:** update its `preferred_brand` and/or `preferred_spec`
     (whichever the user specified). Leave the other fields alone.
   - **If not found:** append a new `{ item, preferred_brand?, preferred_spec?, notes? }`
     entry.
3. `kg_write` the full file. `kg_read` to verify.
4. Reply: "Got it — for X I'll prefer <brand>/<spec>."

Examples:

- "I prefer Nature's Promise for ground chicken" → `preferred_brand:
  "Nature's Promise"` on the ground-chicken entry.
- "For eggs I like cage-free" → `preferred_spec: cage-free` on eggs.
- "Always unsalted butter" → upsert butter with `preferred_spec: unsalted`.

### 4. Upsert pantry

Triggers: "we have a lot of X already", "I just bought a big bag of X",
"add X to my pantry", "I'm stocked on X".

Workflow:

1. `kg_read("pantry.md")`. Parse `items`.
2. Find the existing entry for `<item>`.
   - **If found:** refresh `last_purchased` to today. Keep
     `shelf_life_days` unless the user explicitly changed it.
   - **If not found:** append `{ item, last_purchased: <today>,
     shelf_life_days: <default> }`.
3. `kg_write` the full file. `kg_read` to verify.
4. Reply: "Logged — X (last purchased <today>, shelf life N days). I'll
   skip it on orders until ~<today + 0.8*N>."

**Shelf-life defaults** (use these when the user doesn't specify):

- rice (any): 365
- dry pasta: 730
- canned goods: 730
- paper towels: 180
- OTC meds (ibuprofen, acetaminophen, etc.): 365
- frozen items (meat, veg, pizza): 365
- flour / sugar / baking staples: 365
- coffee beans (sealed): 180
- spices: 365
- anything else dry/shelf-stable: 365

If the item doesn't match any of these and you're unsure, ask the user for
a rough shelf-life in days.

### 5. Add dislike

Triggers: "I don't like brand X", "skip salmon", "no canned Y", "don't get
the low-sodium version".

Workflow:

1. `kg_read("dislikes.md")`. Parse `items`, `brands`, `specs`.
2. **Route by what the user said:**
   - **Brand name** (proper noun, capitalized, e.g. "Tyson", "Whole
     Foods") → append to `brands`.
   - **Food/product** (e.g. "salmon", "kale", "tofu") → append to `items`.
   - **Descriptor or modifier** (e.g. "canned", "low-sodium", "spicy",
     "organic") → append to `specs`.
   - Ambiguous? Ask: "Is 'X' a brand, a food, or a descriptor?"
3. Skip duplicates (case-insensitive).
4. `kg_write` the full file. `kg_read` to verify.
5. Reply: "Added X to dislikes (<items|brands|specs>)."

### 6. Show

Triggers: "show my grocery list", "what are my staples?", "what's in my
pantry?", "what do I dislike?", "show my preferences".

Workflow:

1. `kg_read` the page(s) relevant to the question. (Staples → `staples.md`,
   pantry → `pantry.md`, dislikes → `dislikes.md`, preferences →
   `preferences.md`. "Grocery list" by itself → staples + a one-line
   pointer to the others.)
2. Render as **readable markdown**, not raw YAML. Examples:

   - Staples:

         **Tier 1 (every order):** eggs ×1, whole milk ×1, ground chicken ×1
         **Tier 2 (most orders):** greek yogurt ×2
         **Tier 3 (sometimes):** ice cream ×1

   - Pantry:

         - jasmine rice — last bought 2026-04-12, ~365d shelf life
         - paper towels — last bought 2026-05-01, ~180d shelf life

   - Dislikes:

         **Items:** salmon, kale
         **Brands:** Tyson
         **Specs:** canned, low-sodium

   - Preferences:

         - ground chicken: Nature's Promise, 99% lean
         - eggs: cage-free
         - butter: unsalted

3. Do not call the sidecar for show intents — this is read-only KG access.

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
