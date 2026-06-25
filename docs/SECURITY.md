# Security & secret-hygiene

This repo is intended to be open-source-able. **No personal data, business
data, or secrets may live in tracked source.** This doc is the contract.

## The model: real data is runtime config, git holds only templates

| Kind | Where it lives | In git? |
|---|---|---|
| API keys, tokens, Composio key, Discord bot token | `.env` | ❌ gitignored |
| Discord user creds (bookmarklet) | `discord-creds.json` / OS keyring | ❌ gitignored |
| Recipient email, invoice counter, sending entity | sqlite `data.db` (set via dashboard / `!invoice`) | ❌ gitignored (`*.db`) |
| Invoice identity: name, address, phone, client, rate, gh repo/author | `.env` (`INVOICE_*`) | ❌ gitignored |
| Wiki / people pages (PII) | `wiki/` | ❌ gitignored (except committed grocery KG scaffold) |
| **Placeholders & shape of the above** | `.env.example` | ✅ committed |

Rule of thumb: if a value is specific to a person, a client, money, or grants
access — it goes in `.env` or the DB (both gitignored) and is collected at
runtime (dashboard / CLI / OAuth). Source ships a **placeholder** and reads the
real value from env/config at startup. The generator/daemon must **fail loudly**
when required config is unset, never fall back to a baked-in personal default.

## What counts as sensitive (and what doesn't)

Sensitive — never in source:
- Personal email, phone, home/physical address, legal/business name
- Client names, contract rates, invoice amounts, private client repo slugs
- Any credential/token/key/secret

Not sensitive — fine in a public repo (don't churn these):
- The project's own GitHub repo URL in `Cargo.toml` / `package.json`
- The owner's GitHub *handle* where it's an inherent public identifier
  (it appears on every public commit anyway) or test-fixture data under
  `#[cfg(test)]` / `tests/fixtures/`
- Project codenames in comments (no client/PII attached)

## Guardrail: pre-commit scanner

```
./scripts/install-git-hooks.sh                   # one-time: install the hook
./scripts/check-no-personal-data.sh --tracked    # audit the whole tree anytime
```

The hook blocks staged changes containing secret shapes (PEM keys, `ghp_`,
`xox*-`, `sk-`, `AIza…`, `secret/api_key/token = "…"`), US phone numbers, real
email addresses (anything not `@example.com`/`localhost`), and ever tracking
`.env` / `*.db` / `discord-creds*.json` / `tenant.env` / key files. It is a backstop,
not a substitute for not hardcoding data. Override a confirmed false positive
(e.g. an `*.example` template) with `git commit --no-verify`.

## If something sensitive is committed

Order matters:

1. **Make the repo private immediately** (`gh repo edit <repo> --visibility
   private --accept-visibility-change-consequences`). Stops *further* exposure.
2. **Rotate** any leaked credential — the value is compromised regardless of
   later cleanup; keys/tokens must be regenerated.
3. **Scrub source** — move the data to `.env`/DB, leave a placeholder, land it.
4. **Purge git history** — gitignoring or deleting a value does **not** remove
   it from past commits/clones/forks/caches. Use `git filter-repo` (or BFG) to
   excise the strings, then force-push; coordinate with anyone holding clones.
5. Only then is it safe to make public again — or, simpler, seed a **fresh**
   repo from the scrubbed tree (no history to purge).

## Before flipping a repo public — checklist

- [ ] `./scripts/check-no-personal-data.sh --tracked` passes
- [ ] `git log -p | rg -i '<your email>|<address>|<client>'` is clean (history)
- [ ] `.env`, `*.db`, `discord-creds.json`, and `wiki/` people/PII pages are
      gitignored and not tracked — `wiki/*` is ignored except the committed
      grocery KG scaffold (`wiki/groceries/*.md`)
      (`git ls-files | rg -i 'env|\.db|creds'` → only `.example`)
- [ ] `tenant.env` is not tracked — it isn't in `.gitignore` but the pre-commit
      scanner blocks it by name (`tenant\.env$` in `BANNED_NAMES`)
- [ ] `.env.example` has every required key as a placeholder, no real values
