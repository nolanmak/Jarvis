#!/usr/bin/env python3
"""Secret scrubber for the Apple Notes bundle (#1056).

`scrub(text)` replaces every recognised secret with `[REDACTED:<kind>]` and
returns the findings. `python3 scrub.py --check <bundle>` re-scans a written
bundle and exits 1 on any finding; `sync.py --commit` runs it before staging.

Documented misses (pinned by tests): a bare high-entropy string with no
keyword or known prefix, and a password written in prose. Operators handle
those with the quarantine list in the sync config.
"""
import argparse
import base64
import re
import sys
from collections import namedtuple
from pathlib import Path

Finding = namedtuple("Finding", "kind line")

# Bump when a pattern changes so cached quarantine decisions are re-made.
RULES_VERSION = "2026-09-16.2"

_PEM_BEGIN = re.compile(r"-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----")
_PEM_END = re.compile(r"-----END (?:[A-Z ]+ )?PRIVATE KEY-----")
_AWS_SECRET_HINT = re.compile(r"aws_secret|secret_access|aws secret", re.I)
_AWS_SECRET_VALUE = re.compile(r"(?<![A-Za-z0-9/+=])[A-Za-z0-9/+=]{40}(?![A-Za-z0-9/+=])")
_JWT = re.compile(r"\b(eyJ[A-Za-z0-9_-]{4,})\.([A-Za-z0-9_-]{4,})\.([A-Za-z0-9_-]{4,})\b")

# Order matters: a more specific prefix must precede the generic one it
# would otherwise match (anthropic before openai).
_PATTERNS = [
    ("aws-access-key", re.compile(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("github-token", re.compile(r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})\b")),
    ("slack-token", re.compile(r"\bxox[abpr]-[A-Za-z0-9-]{10,}\b")),
    ("anthropic-key", re.compile(r"\bsk-ant-[A-Za-z0-9_-]{20,}\b")),
    ("openai-key", re.compile(r"\bsk-[A-Za-z0-9_-]{20,}\b")),
    ("stripe-key", re.compile(r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}\b")),
    ("google-api-key", re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b")),
]
# Value-only redaction: keep the keyword so the note still reads sensibly.
_ASSIGNMENT = re.compile(
    r"(?P<key>\b(?:password|passwd|pwd|secret|token|api[_-]?key)\b\s*[:=]\s*)(?P<val>\S{6,})"
    r"|(?P<pkey>\b(?:pin|passcode)\b\s*[:=]\s*)(?P<pval>\d{4,})", re.I
)
# 13–19 digits, optionally grouped by spaces or dashes, Luhn-valid. A bare
# run with no separators also needs a card word on the line, so Luhn-valid
# ids (snowflakes, order numbers) are left alone.
_CARD = re.compile(
    r"(?<![\d-])(?:"
    r"\d{4}[ -]\d{4}[ -]\d{4}[ -]\d{4}(?:[ -]\d{1,3})?"   # 4-4-4-4(-3): Visa, MC, Discover, JCB
    r"|\d{4}[ -]\d{6}[ -]\d{5}"                       # 4-6-5: Amex
    r"|\d{13,19}"                                     # contiguous
    r")(?![\d-])"
)
_CARD_HINT = re.compile(r"\b(?:card|visa|mastercard|amex|discover|cc|cvv|credit|debit)\b", re.I)


def _is_jwt(match):
    head = match.group(1)
    try:
        raw = base64.urlsafe_b64decode(head + "=" * (-len(head) % 4))
    except (ValueError, TypeError):
        return False
    return raw.startswith(b'{"alg"')


def _luhn(digits):
    total = 0
    for i, ch in enumerate(reversed(digits)):
        d = int(ch)
        if i % 2:
            d = d * 2 - 9 if d > 4 else d * 2
        total += d
    return total % 10 == 0


def _card_sub(match, hinted):
    raw = match.group(0)
    digits = re.sub(r"\D", "", raw)
    grouped = raw != digits
    if 13 <= len(digits) <= 19 and _luhn(digits) and (grouped or hinted):
        return "[REDACTED:credit-card]"
    return raw


def _assignment_sub(match):
    if match.group("key") is not None:
        key, val = match.group("key"), match.group("val")
    else:
        key, val = match.group("pkey"), match.group("pval")
    if val.startswith("[REDACTED:"):
        return match.group(0)
    return key + "[REDACTED:password-assignment]"


def _scrub_line(line, prev_line):
    """Return (new_line, kinds_found) for one line; PEM blocks handled by caller."""
    kinds = []
    for kind, pat in _PATTERNS:
        line, n = pat.subn(f"[REDACTED:{kind}]", line)
        if n:
            kinds.append(kind)
    before = line
    line = _JWT.sub(lambda m: "[REDACTED:jwt]" if _is_jwt(m) else m.group(0), line)
    if line != before:
        kinds.append("jwt")
    hinted = _AWS_SECRET_HINT.search(line) or (prev_line is not None and _AWS_SECRET_HINT.search(prev_line))
    if hinted:
        line, n = _AWS_SECRET_VALUE.subn("[REDACTED:aws-secret-key]", line)
        if n:
            kinds.append("aws-secret-key")
    before = line
    hinted = bool(_CARD_HINT.search(line))
    line = _CARD.sub(lambda m: _card_sub(m, hinted), line)
    if line != before:
        kinds.append("credit-card")
    before = line
    line = _ASSIGNMENT.sub(_assignment_sub, line)
    if line != before:
        kinds.append("password-assignment")
    return line, kinds


def scrub(text):
    """Redact secrets in `text`. Returns (scrubbed_text, [Finding, ...])."""
    lines = text.split("\n")
    out = []
    findings = []
    i = 0
    prev = None
    while i < len(lines):
        line = lines[i]
        begin = _PEM_BEGIN.search(line)
        if begin:
            # Everything from BEGIN through the matching END (same line, a
            # later line, or EOF) collapses to one marker; text before BEGIN
            # and after END on their lines is kept.
            head = line[:begin.start()]
            end = _PEM_END.search(line, begin.end())
            j = i
            while end is None and j + 1 < len(lines):
                j += 1
                end = _PEM_END.search(lines[j])
            tail = lines[j][end.end():] if end else ""
            # A trailing escaped newline inside a quoted value belongs to the key.
            if tail.startswith("\\n"):
                tail = tail[2:]
            out.append(head + "[REDACTED:private-key]" + tail)
            findings.append(Finding("private-key", i + 1))
            i = j + 1
            prev = None
            continue
        new, kinds = _scrub_line(line, prev)
        out.append(new)
        findings.extend(Finding(k, i + 1) for k in kinds)
        prev = line
        i += 1
    return "\n".join(out), findings


def check_bundle(root):
    """Yield (path, line, kind) for every finding under root/notes/**/*.md."""
    root = Path(root)
    if not (root / "notes").is_dir():
        return
    for path in sorted((root / "notes").rglob("*.md")):
        _, findings = scrub(path.read_text())
        for f in findings:
            yield path.relative_to(root), f.line, f.kind


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--check", metavar="BUNDLE_ROOT", required=True,
                    help="re-scan a written bundle; exit 1 on any finding")
    args = ap.parse_args(argv)
    hits = list(check_bundle(args.check))
    for path, line, kind in hits:
        print(f"{path}:{line} {kind}")
    return 1 if hits else 0


if __name__ == "__main__":
    sys.exit(main())
