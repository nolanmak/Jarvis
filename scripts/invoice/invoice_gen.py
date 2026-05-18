#!/usr/bin/env python3
"""Weekly invoice generator.

Reads merged PRs (gh) for a Sunday->Sunday week, categorizes them, and renders a
single PDF: Letter, Arial(=Helvetica), INVOICE title, FROM / BILL TO / meta
blocks, SERVICES & GOODS body at 8.5pt, divider + pinned Total / Billed /
Total Due line at the bottom of the last page.

All personal/business data (FROM/BILL-TO/rate/repo/author) comes from env —
see .env.example. Nothing personal is hardcoded.

Usage:
  invoice_gen.py --number 29 --start 2026-04-05 --end 2026-04-12 \
                 --prs /tmp/prs.tsv --out /path/Invoice_29.pdf
"""
import argparse, datetime, subprocess, json, sys, os
from reportlab.pdfgen import canvas
from reportlab.lib.pagesizes import letter
from reportlab.pdfbase.pdfmetrics import stringWidth

W, H = letter                       # 612 x 792
LEFT = 72.0                         # 1" left margin (matches sample)
REG, BOLD = "Helvetica", "Helvetica-Bold"

# ---- billing config — read from env (.env, which is gitignored). NEVER ----
# hardcode personal/business data here. See .env.example for the template.
# INVOICE_FROM_LINES is pipe-separated: "Business|Name|Street|City, ST ZIP|Phone".
HOURS = int(os.environ.get("INVOICE_HOURS", "0") or "0")
RATE = int(os.environ.get("INVOICE_RATE", "0") or "0")
TOTAL_DUE = f"${HOURS * RATE:,}"
FROM_LINES = [s for s in os.environ.get("INVOICE_FROM_LINES", "Your Name").split("|") if s]
BILL_TO = os.environ.get("INVOICE_BILL_TO", "Client Name")

# ---- closing-block coordinates (pinned to bottom, from sample) ----
DIV_TOP, TOTAL_TOP, BILLED_TOP, DUE_TOP = 658.7, 670.0, 681.2, 704.8
BODY_FLOOR = DIV_TOP - 11.0         # body must stop above the pinned block

CAT_ORDER = ["Features", "Bug Fixes", "Refactor", "Cleanup", "Docs", "Tests", "Other"]
CAT_SHORT = {"Features": "features", "Bug Fixes": "fixes", "Refactor": "refactor",
             "Cleanup": "chore", "Docs": "docs", "Tests": "tests", "Other": "other"}


def categorize(title: str) -> str:
    t = title.strip().lower()
    if t.startswith(("fix:", "fix(", "(fix)", "bugfix", "hotfix", "patch")) or t.startswith("fix "):
        return "Bug Fixes"
    if t.startswith(("refactor", "restructure")):
        return "Refactor"
    if t.startswith(("docs:", "docs(")):
        return "Docs"
    if t.startswith(("test:", "tests:", "test(")):
        return "Tests"
    if t.startswith(("cleanup", "chore:", "chore(", "remove", "delete", "deprecate", "trim")):
        return "Cleanup"
    if t.startswith(("feat:", "feat(", "feature", "add ", "implement", "new ")):
        return "Features"
    return "Other"


def wrap(text, font, size, max_w, indent=0.0):
    """Greedy word-wrap; continuation lines get a hanging indent."""
    words, lines, cur = text.split(), [], ""
    for w in words:
        trial = (cur + " " + w).strip()
        avail = max_w - (indent if lines else 0)
        if stringWidth(trial, font, size) <= avail or not cur:
            cur = trial
        else:
            lines.append(cur)
            cur = w
    if cur:
        lines.append(cur)
    return lines


# GitHub repo to read merged PRs from (owner/name slug) — `gh --repo` so NO
# local clone is needed. Set INVOICE_GH_REPO in .env (gitignored); the legacy
# ORCHID_GH_REPO name is still honored. No hardcoded default — a public repo
# must not name a private client project.
DEFAULT_GH_REPO = os.environ.get("INVOICE_GH_REPO") or os.environ.get("ORCHID_GH_REPO", "")
# GitHub login whose merged PRs are billed. Set INVOICE_GH_AUTHOR in .env.
GH_AUTHOR = os.environ.get("INVOICE_GH_AUTHOR", "")


def fetch_prs(gh_repo, tsv):
    """Return list of (date 'YYYY-MM-DD', '#NNNN', title). Prefer tsv cache.

    `gh_repo` is an owner/name slug; `gh` is invoked with `--repo` so no
    local checkout of the source repo is required on the host.
    """
    rows = []
    if tsv and os.path.exists(tsv):
        for ln in open(tsv):
            p = ln.rstrip("\n").split("\t")
            if len(p) >= 3:
                rows.append((p[0], p[1], p[2]))
        return rows
    repo = gh_repo or DEFAULT_GH_REPO
    if not repo or not GH_AUTHOR:
        sys.exit(
            "invoice_gen: set INVOICE_GH_REPO and INVOICE_GH_AUTHOR in .env "
            "(or pass a --prs tsv cache). Refusing to guess."
        )
    jq = (
        '.[]|select(.author.login=="%s")|'
        r'"\(.mergedAt[:10])\t#\(.number)\t\(.title)"' % GH_AUTHOR
    )
    raw = subprocess.check_output(
        ["gh", "pr", "list", "--repo", repo,
         "--state", "merged", "--limit", "800",
         "--json", "number,title,mergedAt,author",
         "--jq", jq],
        text=True)
    for ln in raw.splitlines():
        p = ln.split("\t")
        if len(p) >= 3:
            rows.append((p[0], p[1], p[2]))
    return rows


def mdy(d, pad_day=True):           # date -> M/DD/YYYY (sample style)
    return f"{d.month}/{d.day:02d}/{d.year}" if pad_day else f"{d.month}/{d.day}/{d.year}"


def build(number, start, end, rows, out_path, invoice_date=None):
    start_d = datetime.date.fromisoformat(start)
    end_d = datetime.date.fromisoformat(end)
    # Invoice Date = the day the invoice is generated (today), NOT the work-week
    # end. Net-30 due date is counted from the invoice date.
    inv_d = datetime.date.fromisoformat(invoice_date) if invoice_date else datetime.date.today()
    due_d = inv_d + datetime.timedelta(days=30)

    # bucket: (start, end] — matches the contiguous-weekly sample convention
    wk = [(d, n, t) for (d, n, t) in rows if start < d <= end]
    cats = {c: [] for c in CAT_ORDER}
    for d, n, t in wk:
        cats[categorize(t)].append((d, n, t))
    for c in cats:
        cats[c].sort(key=lambda r: r[0], reverse=True)   # newest first

    c = canvas.Canvas(out_path, pagesize=letter)

    def text(top, x, s, font, size):
        c.setFont(font, size)
        c.drawString(x, H - top - size, s)

    # ---- header blocks (fixed coords from sample) ----
    text(73.8, LEFT, "INVOICE", BOLD, 12)
    text(103.5, LEFT, "FROM:", BOLD, 11)
    top = 130.4
    for ln in FROM_LINES:
        text(top, LEFT, ln, REG, 11)
        top += 14.8
    text(225.4, LEFT, "BILL TO:", BOLD, 11)
    text(243.0, LEFT, BILL_TO, REG, 11)
    meta = [f"Invoice #: {number}",
            f"Invoice Date: {mdy(inv_d)}",
            f"Due Date: {mdy(due_d, pad_day=False)}",
            "Terms: Net 30"]
    top = 264.9
    for ln in meta:
        text(top, LEFT, ln, REG, 11)
        top += 12.6
    text(343.0, LEFT, "SERVICES & GOODS", BOLD, 11)

    # ---- body (8.5pt, leading 11.25, paginates) ----
    SZ, LEAD = 8.5, 11.25
    MAXW = W - LEFT - 72.0           # right margin 1"
    cur = 355.2

    def newpage_if_needed(next_lines=1):
        nonlocal cur
        if cur + next_lines * LEAD > BODY_FLOOR:
            c.showPage()
            cur = 72.0               # continuation page top

    rng = f"{start_d:%m/%d/%Y} to {end_d:%m/%d/%Y}"
    text(cur, LEFT, f"Invoice Summary: {rng}   {HOURS} hours", BOLD, SZ)
    cur += LEAD * 1.5

    total = 0
    breakdown = []
    for cat in CAT_ORDER:
        items = cats[cat]
        if not items:
            continue
        total += len(items)
        breakdown.append(f"{len(items)} {CAT_SHORT[cat]}")
        newpage_if_needed(2)
        text(cur, LEFT, f"{cat} — Merged ({len(items)})", BOLD, SZ)
        cur += LEAD
        for d, n, t in items:
            dd = datetime.date.fromisoformat(d)
            line = f"- {dd:%m/%d} | {n} | {t.strip()}"
            for i, seg in enumerate(wrap(line, REG, SZ, MAXW, indent=10)):
                newpage_if_needed(1)
                text(cur, LEFT + (10 if i else 0), seg, REG, SZ)
                cur += LEAD
        cur += LEAD * 0.4            # small gap between categories

    if total == 0:                   # safety: never silently empty
        text(cur, LEFT, "- (no merged PRs in range — see dev-branch work detail)", REG, SZ)

    # ---- pinned closing block (bottom of last page) ----
    text(DIV_TOP, 76.7, "---", REG, SZ)
    bd = ", ".join(breakdown) if breakdown else "n/a"
    text(TOTAL_TOP, 76.7, f"Total: {total} merged PRs ({bd})", REG, SZ)
    text(BILLED_TOP, 76.7, f"Billed: {HOURS} hours", REG, SZ)
    base_y = H - DUE_TOP - 11
    c.setFillColorRGB(0, 0, 0)
    c.circle(93.0, base_y + 3.4, 3.3, stroke=0, fill=1)   # bullet (drawn, font-safe)
    c.setFont(BOLD, 11)
    c.drawString(108.0, base_y, f"Total Due: {TOTAL_DUE}   Thank you! - Nolan")

    c.showPage()
    c.save()
    return number, rng, total, out_path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--number", type=int, required=True)
    ap.add_argument("--start", required=True)
    ap.add_argument("--end", required=True)
    ap.add_argument("--prs", default="")
    ap.add_argument("--gh-repo", default=DEFAULT_GH_REPO,
                    help="owner/name slug to read merged PRs from (no clone needed)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--invoice-date", default="",
                    help="YYYY-MM-DD shown as Invoice Date (default: today). Due = +30d.")
    a = ap.parse_args()
    rows = fetch_prs(a.gh_repo, a.prs)
    n, rng, tot, path = build(a.number, a.start, a.end, rows, a.out,
                              invoice_date=a.invoice_date or None)
    print(f"#{n}  {rng}  {tot} PRs  -> {path}")


if __name__ == "__main__":
    main()
