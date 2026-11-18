#!/usr/bin/env python3
"""Export a private 30-day owner-alert counterfactual snapshot, never send.

Run against the live DB ONLY with SQLite URI mode=ro (enforced here). Pipe
stdout over SSH into a mode-0600 file on the development machine; replay there:
  augmentagent imessage alerts calibrate --input snapshot.jsonl
Only aggregate replay results belong in issues. Delete the private export after
use. No subjects are exported. Message bodies/identities remain private inputs.
The baseline is recorded High/Critical alert outcomes, not rerun model output;
the revised result adds deterministic owner gates, holding legacy outcomes fixed.
"""
import argparse
import datetime
import json
from pathlib import Path
import re
import sqlite3


def export(db, wiki, days, output, until_ms=None):
    until = until_ms if until_ms is not None else int(datetime.datetime.now(datetime.timezone.utc).timestamp() * 1000)
    since = until - days * 86_400_000
    connection = sqlite3.connect(Path(db).resolve().as_uri() + '?mode=ro')
    try:
        connection.execute('PRAGMA query_only=ON')
        connection.execute('BEGIN')
        priorities = connection.execute('SELECT sender,urgency FROM owner_alert_priorities ORDER BY sender').fetchall()
        people = []
        root = Path(wiki).resolve()
        for page in sorted((root / 'people').glob('*.md')):
            if not page.resolve().is_relative_to(root) or not re.fullmatch(r'[A-Za-z0-9_-]+', page.stem):
                continue
            text = page.read_text()
            # Keep identity frontmatter and explicitly typed relationship lines;
            # no unrelated person prose, subjects or historical messages.
            lines = text.splitlines()
            context = []
            if lines and lines[0].strip() == '---':
                end = next((i for i in range(1, len(lines)) if lines[i].strip() == '---'), None)
                if end is not None:
                    context = lines[:end + 1]
                    lines = lines[end + 1:]
            context.extend(line for line in lines if re.match(r'^\s*(?:[-*]\s*)?relationship\s*:', line, re.I)
                           or re.search(r'active[- ]collaborator|close[- ]contact', line, re.I))
            people.append({'slug': page.stem, 'context': '\n'.join(context) + '\n'})
        output.write(json.dumps({'schema': 1, 'since_ms': since, 'until_ms': until,
                                 'priorities': priorities, 'people': people}) + '\n')
        # All triaged email with retained bodies, once per source message. Old
        # deployments may have no calibration_inputs row, but retain processed
        # emails and owner_alert_assessments. Union includes both cohorts.
        query = '''SELECT e.fromEmail,e.body,
            EXISTS(SELECT 1 FROM owner_alerts a WHERE a.id='gmail:'||e.messageId AND a.urgency IN ('high','critical'))
            FROM emails e WHERE e.platform IN ('gmail','email') AND e.messageId IN (
            SELECT message_id FROM owner_alert_assessments WHERE assessed_at_ms>=:since AND assessed_at_ms<:until
            UNION SELECT message_id FROM triage_calibration_inputs WHERE observed_at_ms>=:since AND observed_at_ms<:until)
            ORDER BY e.messageId'''
        for sender, body, before in connection.execute(query, {"since": since, "until": until}):
            output.write(json.dumps({'from': sender or '', 'body': body or '', 'before_high': bool(before)}) + '\n')
    finally:
        connection.close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--db', required=True)
    parser.add_argument('--wiki', required=True)
    parser.add_argument('--days', type=int, default=30)
    args = parser.parse_args()
    if not 1 <= args.days <= 365:
        parser.error('--days must be between 1 and 365')
    import sys
    export(args.db, args.wiki, args.days, sys.stdout)
