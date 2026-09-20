# Security & secret-hygiene

This repo is intended to be open-source-able. **No personal data, business
data, or secrets may live in tracked source.** This doc is the contract.

## The model: real data is runtime config, git holds only templates

| Kind | Where it lives | In git? |
|---|---|---|
| API keys, tokens, Composio key, Discord bot token | `.env` | ❌ gitignored |
| Discord user creds (bookmarklet) | OS keyring (optional 0600 file only at an out-of-repo `AUGMENTAGENT_DISCORD_CREDS` path) | ❌ never written to the repo |
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
Never use that marker for a real person or live credential. Addresses at
common personal mailbox providers are rejected even with that marker, and
security documentation is scanned too. The CI workflow
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

## Public issue reports

Report software behavior using synthetic examples. Do not include real recipient
lists, message IDs, subjects, private message excerpts, personal names, phone
numbers, local paths, or credentials from the inbox or wiki. Use reserved
`example.com` addresses. The query agent's `aa-gh` shim checks explicit issue
titles and bodies for obvious emails and credentials before posting. It cannot
detect every private fact; review the content before requesting a report.
Reports require an explicit `--body` or `--body-file`; interactive editors and
templates are refused because their final contents cannot be checked first.

Automatic code-mode failure reports publish only a fixed stage, an allowlisted
channel, repair status and fallback mode. Raw generated programs, error strings,
model labels and message/action identifiers remain out of the public report.
The original failed program remains in the private action store for diagnosis.
Query-agent and maintenance-agent prompts require synthetic technical summaries;
quoted user requests and private message context do not belong in public reports.

The Rust issue publisher, research issue filer and maintenance issue/PR writes
also validate titles and bodies before spawning GitHub commands. Recognizable
emails, phone numbers, token shapes and raw diagnostic identifiers cause a
refusal with values withheld. Reserved example domains remain valid fixtures.

## CCat public-push gate

CCat is an optional, fail-closed semantic review gate for explicitly configured
public remotes. It does not replace deterministic scanning: the pre-push gate
runs `check-no-personal-data.sh` before sending a redacted diff representation
to SeaCat. A local secret/PII finding means SeaCat is not called.

Enable it only after setting `SEACAT_API_KEY`,
`AUGMENTAGENT_CCAT_ENABLED=true`, and a comma-separated list of public remote
names in `AUGMENTAGENT_CCAT_PUBLIC_REMOTES`, then run
`scripts/install-git-hooks.sh`. CCat `allow` permits the push; `review` needs a
single-use CCAP receipt bound to the exact ref and redacted payload hash;
`block`, invalid provider data, timeout, and missing configuration stop it.
Neither the raw diff nor SeaCat's raw response is stored in the receipt.

CCAP receipts are HMAC-SHA256 signed with the separate owner-only
`CCAT_APPROVAL_SIGNING_KEY`, expire after five minutes, and are atomically
consumed when the gate uses them. After inspecting a `review` outcome, the
operator creates one with `augmentagent-ccat receipt-create <payload-hash>
<local-sha> <remote-ref> <new-private-receipt-path>`, then retries the push
with `CCAT_APPROVAL_RECEIPT` pointing to that file. The hook verifies it using
`receipt-verify`; unsigned, expired, mismatched, or reused files fail closed.

Git hooks protect configured workstations and agent paths; `git push
--no-verify` and an uncontrolled clone can bypass them. GitHub Actions cannot
prevent a public ref from being received because they run after push. Keep
default branches protected, restrict direct-push permissions, and enable
GitHub secret-scanning push protection where available. CCat is not proof of
factual correctness, legal compliance, or exhaustive PII detection.

The Codex tool bridge refuses direct `git push`: its ordinary Git sandbox
disables repository hooks, so allowing that operation would bypass the gate.

Before changing a CCat public-push threshold, label a decision run and verify
it locally with `scripts/ccat-calibrate.py --input <run.jsonl>`. A public-push
threshold requires at least 100 labelled examples and zero false allows. The
run file is private operational evidence: it must contain only case IDs and
outcomes, never source text, diffs, identifiers, or provider prompts.
`AUGMENTAGENT_CCAT_CALIBRATION_REPORT` must point to that verified private
JSONL file before the public-push hook will make a provider request.

Jarvis's autonomous self-improvement publisher performs the same check before
any configured-public `git push`: it refuses if the installed common Git hook
does not invoke `ccat-public-push-gate.sh`. This preserves the normal hook's
deterministic scan, calibration check, CCat call, and receipt verification.
