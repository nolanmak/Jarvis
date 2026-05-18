# Weekly invoice tooling

Self-contained generator + sender. The Rust layer (CLI `invoice` subcommand +
weekly Sunday scheduler + Discord recipient command) shells out to this and owns
durable state (recipient, sequential counter, last-billed week).

**No personal/business data is hardcoded.** Identity, client, rate, repo, and
author all come from env (set in `.env`, which is gitignored — see
`.env.example`). The recipient email is set at runtime via the dashboard or
`!invoice recipient` and stored in the gitignored sqlite DB.

## Files
- `invoice_gen.py` — renders the PDF (offline, reportlab). Invoice Date =
  generation day; Due = +30d; Summary line = work week.
- `send_invoice.py` — generates then emails the PDF as a real attachment via the
  Composio Python SDK (auto file-upload). Dry-run by default.
- `requirements.txt` — `pip install -r requirements.txt`

## Conventions
- Weeks are Sunday → Sunday, bucketed `(start, end]` by PR **merge date**.
- Billing quantity/rate come from `INVOICE_HOURS` / `INVOICE_RATE`.
- Source of truth: merged PRs by `INVOICE_GH_AUTHOR` in `INVOICE_GH_REPO` (`gh`).

## Env (all in `.env` — gitignored, never commit real values)
| var | purpose |
|-----|---------|
| `COMPOSIO_API_KEY` | required to actually send |
| `INVOICE_FROM_ENTITY` | Composio entity/user_id for the sending account |
| `INVOICE_FROM_LINES` | pipe-separated `Business\|Name\|Street\|City, ST ZIP\|Phone` |
| `INVOICE_BILL_TO` | client / bill-to name |
| `INVOICE_HOURS`, `INVOICE_RATE` | billing quantity + hourly rate |
| `INVOICE_GH_REPO` | owner/name slug for `gh` (legacy alias: `ORCHID_GH_REPO`) |
| `INVOICE_GH_AUTHOR` | GitHub login whose merged PRs are billed |
| `INVOICE_OUT_DIR` | PDF output dir (default `/tmp/invoices`) |

> Portable: PR data is read via `gh pr list --repo <INVOICE_GH_REPO>`, so the
> host only needs an authenticated `gh` — the source repo is **not** checked
> out on the daemon machine. The generator exits loudly if repo/author are
> unset rather than guessing.

## Examples
```bash
# generate only
python3 invoice_gen.py --number 35 --start 2026-05-17 --end 2026-05-24 \
    --out /tmp/Invoice_35.pdf

# dry-run send (no email)
python3 send_invoice.py --number 35 --start 2026-05-17 --end 2026-05-24 \
    --to you@example.com --from-entity <entity>

# real send
python3 send_invoice.py --number 35 --start 2026-05-17 --end 2026-05-24 \
    --to you@example.com --from-entity <entity> --dry-run false
```
