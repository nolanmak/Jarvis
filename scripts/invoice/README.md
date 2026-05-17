# Weekly Orchid invoice tooling

Self-contained generator + sender. The Rust layer (CLI `invoice` subcommand +
weekly Sunday scheduler + Discord recipient command) shells out to this and owns
durable state (recipient, sequential counter from #35, last-billed week).

## Files
- `invoice_gen.py` — renders the PDF (offline, reportlab). Matches sample #28
  layout. Invoice Date = generation day; Due = +30d; Summary line = work week.
- `send_invoice.py` — generates then emails the PDF as a real attachment via the
  Composio Python SDK (auto file-upload). Dry-run by default.
- `requirements.txt` — `pip install -r requirements.txt`

## Conventions
- Weeks are Sunday → Sunday, bucketed `(start, end]` by PR **merge date**.
- Billing: flat 50 h @ $55 = **$2,750**/week (from sample #28).
- Source of truth: merged PRs by `nolanmak` in the **Orchid** repo (`gh`).

## Env
| var | purpose |
|-----|---------|
| `COMPOSIO_API_KEY` | required to actually send |
| `INVOICE_FROM_ENTITY` | Composio entity/user_id for the sending Gmail (n.makatche@gmail.com) |
| `ORCHID_GH_REPO` | owner/name slug for `gh` (default `OrchidStudio/orchid`; **no local clone needed**) |
| `INVOICE_OUT_DIR` | PDF output dir (default `/tmp/invoices`) |

> Portable by design: PR data is read via `gh pr list --repo OrchidStudio/orchid`,
> so the host only needs an authenticated `gh` — the Orchid repo is **not**
> checked out on the daemon machine.

## Examples
```bash
# generate only
python3 invoice_gen.py --number 35 --start 2026-05-17 --end 2026-05-24 \
    --out /tmp/Orchid_Invoice_35.pdf

# dry-run send (no email)
python3 send_invoice.py --number 35 --start 2026-05-17 --end 2026-05-24 \
    --to n.makatche@gmail.com --from-entity <entity>

# real send
python3 send_invoice.py --number 35 --start 2026-05-17 --end 2026-05-24 \
    --to n.makatche@gmail.com --from-entity <entity> --dry-run false
```
