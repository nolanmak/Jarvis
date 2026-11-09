#!/usr/bin/env python3
"""Regenerate/check public fallback names; never copy dotenv values."""
import argparse
import re
import sys
from pathlib import Path


def inventory(template):
    keys = set()
    for raw in template.splitlines():
        line = raw.lstrip()
        body = (line[1:] if line.startswith('#') else line).strip()
        key, separator, _ = body.partition('=')
        if separator and re.fullmatch(r'[A-Z_][A-Z0-9_]*', key):
            keys.add(key)
    if not keys:
        raise ValueError('canonical template has no configuration keys')
    return ''.join(key + '=\n' for key in sorted(keys))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--write', action='store_true')
    mode.add_argument('--check', action='store_true')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[3]
    target = repo / 'crates/augmentagent-cli/src/env_keys.txt'
    try:
        expected = inventory((repo / '.env.example').read_text(encoding='utf-8'))
        if args.write:
            target.write_text(expected, encoding='utf-8')
        elif target.read_text(encoding='utf-8') != expected:
            print('env key inventory is stale; run this tool with --write', file=sys.stderr)
            return 1
    except (OSError, ValueError) as error:
        print('env key inventory unavailable: ' + str(error), file=sys.stderr)
        return 1
    print('env key inventory current (' + str(len(expected.splitlines())) + ' keys)')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
