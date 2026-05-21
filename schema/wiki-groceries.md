# Schema: wiki/groceries/

Frontmatter contract for the grocery knowledge graph. The agent reads and
writes these via the `grocery` tool's `kg_*` and `record_order` actions
(see `src/groceryKg.ts`).

All files are markdown with YAML frontmatter delimited by `---` lines.
Paths are relative to `wiki/groceries/`.

## staples.md

```yaml
type: grocery-staples
last_recomputed: <YYYY-MM-DD | null>
tiers:
  "1_every_order":
    - { item: <name>, qty: <number>, prodId?: <number>, brand?: <string> }
  "2_most_orders": [...]
  "3_sometimes":   [...]
```

Tier assignment is recomputed from the trailing 12 entries in `orders/`
after each new order (see `skills/grocery/SKILL.md` for the rules).

## preferences.md

```yaml
type: grocery-preferences
items:
  - item: <name>
    preferred_brand?: <string>
    preferred_spec?: <string>
    notes?: <string>
```

## pantry.md

```yaml
type: grocery-pantry
items:
  - item: <name>
    last_purchased: <YYYY-MM-DD>
    shelf_life_days: <number>
```

The agent skips items where `today < last_purchased + shelf_life_days * 0.8`.

## dislikes.md

```yaml
type: grocery-dislikes
items:  [<name>, ...]
brands: [<string>, ...]
specs:  [<string>, ...]   # substring match against product name/description
```

## orders/<YYYY-MM-DD>.md

```yaml
type: grocery-order
date: <YYYY-MM-DD>
approved: <boolean>
total?: <number>
```

Body sections (markdown, written by `recordOrder()` in `src/groceryKg.ts`):

- `## Items ordered` — `- N× <name> (<brand>) — $<price> [prodId: <id>]`
- `## Out of stock` — `- <name> — <reason>`
- `## Substitutions` — `- <from> → <to>`
- `## Feedback` — free text captured from the Discord approval card

## Constraints

- All paths must end in `.md`.
- All KG writes are confined to `wiki/groceries/` (see
  `resolveSafe()` in `src/groceryKg.ts`); `..` traversal is rejected.
- The agent tool path-restricts the same way; nothing outside this folder
  is reachable via `kg_read` / `kg_write` / `kg_append`.
