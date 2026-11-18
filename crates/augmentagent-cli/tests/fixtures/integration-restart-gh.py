#!/usr/bin/python3
"""Read-only scripted boundary for the integration scheduler crash fixture."""
import json, os, pathlib, sys
root = pathlib.Path(os.environ['AUTOSHIP_INTEGRATION_RESTART_ROOT'])
rows = json.loads((root / 'rows.json').read_text())
a = sys.argv[1:]
if a[:2] == ['repo', 'view']:
    print('owner/repo')
elif a[:2] == ['pr', 'list']:
    print(json.dumps(rows))
elif a[:2] == ['issue', 'list']:
    print('[]')
elif a[:2] == ['pr', 'view']:
    row = next(r for r in rows if r['number'] == int(a[2]))
    print(json.dumps(dict(row, labels=[])))
elif a[:1] == ['api'] and '/issues/' in a[1]:
    print(json.dumps(dict(number=int(a[1].rsplit('/', 1)[1]), labels=[])))
else:
    raise AssertionError('unexpected or mutating GitHub request: ' + repr(a))
