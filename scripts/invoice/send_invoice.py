#!/usr/bin/env python3
"""Generate a weekly Orchid invoice PDF and email it as an attachment.

Self-contained unit used by both the backlog send and the weekly Sunday duty.
PDF generation is offline (reportlab); sending uses the Composio Python SDK with
auto file-upload, so the local PDF is attached for real (no raw-HTTP s3key dance).

The Rust layer owns durable state (recipient, sequential counter, last-billed
week) and passes it in explicitly. Standalone/backlog use passes it by hand.

  # dry-run (default): generate + print, send nothing
  send_invoice.py --number 35 --start 2026-05-17 --end 2026-05-24 \
      --to REDACTED --from-entity <composio_entity>

  # actually send
  send_invoice.py ... --dry-run false

Env:
  COMPOSIO_API_KEY   required to send (not for dry-run)
  ORCHID_GH_REPO     owner/name slug for `gh` (default REDACTED; no clone needed)
"""
import argparse, datetime, os, sys, pathlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from invoice_gen import build, fetch_prs, DEFAULT_GH_REPO  # noqa: E402

GMAIL_ACTION = "GMAIL_SEND_EMAIL"


def fmt(d: datetime.date) -> str:        # MM/DD/YYYY, matches the PDF summary
    return f"{d.month:02d}/{d.day:02d}/{d.year}"


def email_body(start_d, end_d) -> str:
    rng = f"{fmt(start_d)}-{fmt(end_d)}"
    return f"Hello,\n\nHere is my weekly invoice for {rng}.\n\nThank you!"


def _composio_client(api_key):
    try:
        from composio import Composio
    except ImportError as e:
        raise SystemExit(
            "composio SDK not installed. `pip install composio` "
            f"(import error: {e})")
    return Composio(
        api_key=api_key,
        dangerously_allow_auto_upload_download_files=True,
    )


def list_accounts(api_key):
    """Print Composio-connected accounts (id / toolkit / status / email-ish)
    so the user can pick the Gmail sending entity. Defensive across SDK
    versions — falls back to dumping raw objects."""
    client = _composio_client(api_key)
    try:
        resp = client.connected_accounts.list()
    except Exception as e:                      # noqa: BLE001
        raise SystemExit(f"could not list connected accounts: {e}")
    items = getattr(resp, "items", None)
    if items is None and isinstance(resp, dict):
        items = resp.get("items") or resp.get("data")
    if items is None:
        items = resp if isinstance(resp, (list, tuple)) else [resp]
    print("Composio connected accounts:")
    for it in items:
        d = it if isinstance(it, dict) else getattr(it, "__dict__", {}) or {}
        cid = d.get("id") or getattr(it, "id", "?")
        tk = d.get("toolkit") or d.get("appName") or getattr(it, "toolkit", "?")
        st = d.get("status") or getattr(it, "status", "?")
        blob = repr(d) if d else repr(it)
        email = next((tok for tok in blob.replace("'", " ").split()
                      if "@" in tok and "." in tok), "(email not exposed)")
        print(f"  - id={cid}  toolkit={tk}  status={st}  email~={email}")
    print("\nThen lock it in:  augmentagent invoice set-entity --entity <id>")


def send_via_composio(api_key, from_entity, to, subject, body, pdf_path):
    """Send through the Composio Python SDK with auto file-upload enabled."""
    client = _composio_client(api_key)
    result = client.tools.execute(
        GMAIL_ACTION,
        user_id=from_entity,
        arguments={
            "recipient_email": to,
            "subject": subject,
            "body": body,
            "is_html": False,
            "attachment": str(pdf_path),   # SDK uploads + rewrites to s3 descriptor
        },
    )
    ok = bool(getattr(result, "successful", None)
              or (isinstance(result, dict) and result.get("successful")))
    if not ok:
        raise SystemExit(f"Composio send failed: {result}")
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--list-accounts", action="store_true",
                    help="list Composio-connected accounts and exit")
    ap.add_argument("--number", type=int)
    ap.add_argument("--start", help="work-week start (Sunday) YYYY-MM-DD")
    ap.add_argument("--end", help="work-week end (Sunday) YYYY-MM-DD")
    ap.add_argument("--to", help="recipient email")
    ap.add_argument("--from-entity", default=os.environ.get("INVOICE_FROM_ENTITY", ""),
                    help="Composio user_id/entity for the sending Gmail account")
    ap.add_argument("--invoice-date", default="",
                    help="YYYY-MM-DD shown as Invoice Date (default: today). Due = +30d.")
    ap.add_argument("--gh-repo", default=DEFAULT_GH_REPO,
                    help="owner/name slug to read merged PRs from (no clone needed)")
    ap.add_argument("--prs", default="", help="optional cached PR tsv")
    ap.add_argument("--out", default="", help="PDF output path (default: temp dir)")
    ap.add_argument("--dry-run", default="true",
                    help="true (default) = generate only, do not send")
    a = ap.parse_args()

    if a.list_accounts:
        key = os.environ.get("COMPOSIO_API_KEY", "")
        if not key:
            raise SystemExit("COMPOSIO_API_KEY not set; cannot list accounts.")
        list_accounts(key)
        return

    missing = [f for f in ("number", "start", "end", "to")
               if getattr(a, f) in (None, "")]
    if missing:
        raise SystemExit(f"missing required args: {', '.join('--' + m for m in missing)}")

    start_d = datetime.date.fromisoformat(a.start)
    end_d = datetime.date.fromisoformat(a.end)
    dry = a.dry_run.lower() not in ("false", "0", "no")

    out = a.out or str(pathlib.Path(
        os.environ.get("INVOICE_OUT_DIR", "/tmp/invoices"))
        / f"Orchid_Invoice_{a.number}.pdf")
    pathlib.Path(out).parent.mkdir(parents=True, exist_ok=True)

    rows = fetch_prs(a.gh_repo, a.prs)
    n, rng, tot, path = build(a.number, a.start, a.end, rows, out,
                              invoice_date=a.invoice_date or None)

    subject = f"Invoice #{n} — {fmt(start_d)}-{fmt(end_d)} (Orchid / REDACTED)"
    body = email_body(start_d, end_d)

    print(f"Invoice #{n}: {rng}  {tot} PRs  -> {path}")
    print(f"  To:      {a.to}")
    print(f"  Subject: {subject}")
    print(f"  Body:    {body!r}")

    if dry:
        print("  [dry-run] not sent. Re-run with --dry-run false to send.")
        return

    api_key = os.environ.get("COMPOSIO_API_KEY", "")
    if not api_key:
        raise SystemExit("COMPOSIO_API_KEY not set; cannot send.")
    if not a.from_entity:
        raise SystemExit("--from-entity (Composio sending account) required to send.")
    send_via_composio(api_key, a.from_entity, a.to, subject, body, path)
    print(f"  ✅ sent to {a.to}")


if __name__ == "__main__":
    main()
