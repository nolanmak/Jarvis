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
| Wiki / people pages (PII) | `wiki/` | ❌ gitignored |
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

The hook scans staged file contents, including renames, and blocks files
containing secret shapes (PEM keys, `ghp_`,
`xox*-`, `sk-`, `AIza…`, `secret/api_key/token = "…"`), US phone numbers, real
email addresses outside reserved example domains and protocol identifiers,
and tracking
`.env` / `*.db` / `*creds*.json` / `tenant.env` / key files. It is a backstop,
not a substitute for not hardcoding data. Templates are scanned too. Prefer
reserved example domains; use an inline
`pii-ok` marker only for a reviewed synthetic credential or service identifier.
Never use that marker for a real person or live credential. The CI workflow
also runs Gitleaks independently of these markers. Findings report locations
without printing matched values.

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
5. Review GitHub-managed PR refs, issue comments, and retained artifacts before
   making the repo public again. Branch rewriting alone does not clean those
   surfaces. A **fresh** repo from the scrubbed tree avoids carrying old Git
   history forward, but cannot recall existing copies.

## Before flipping a repo public — checklist

- [ ] `./scripts/check-no-personal-data.sh --tracked` passes
- [ ] `git log -p | rg -i '<your email>|<address>|<client>'` is clean (history)
- [ ] `.env`, `*.db`, `discord-creds.json`, `wiki/`, `tenant.env` are gitignored
      and not tracked (`git ls-files | rg -i 'env|\.db|creds'` → only `.example`)
- [ ] `.env.example` has every required key as a placeholder, no real values
