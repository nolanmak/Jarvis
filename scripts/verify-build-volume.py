#!/usr/bin/env python3
"""Read-only post-rollout check. Attach its output to #2158 after merging."""
import argparse
import json
import os
from pathlib import Path


def verify(status, volume, device, old_free, before_free):
    expected = {
        'gate output': volume / 'augmentagent-gate-target',
        'session output parent': volume / 'augmentagent-session-targets',
        'verifier output parent': volume / 'augmentagent-verifier-targets',
    }
    for role, path in expected.items():
        label = f'{role} ({path})'
        if not any(row['dev'] == device and label in row['labels'] for row in status):
            raise ValueError(f'volume_verification_missing_output: {label}')
    if old_free <= before_free:
        raise ValueError('volume_verification_no_reclamation: old volume free bytes have not risen; retry after idle prune')
    return {'verified_volume': str(volume), 'old_before_free_bytes': before_free,
            'old_after_free_bytes': old_free, 'reclaimed_net_bytes': old_free - before_free}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--volume', type=Path, required=True)
    parser.add_argument('--old', type=Path, required=True)
    parser.add_argument('--before-free-bytes', type=int, required=True)
    parser.add_argument('--status-json', type=Path, required=True)
    args = parser.parse_args()
    try:
        stats = os.statvfs(args.old)
        result = verify(json.loads(args.status_json.read_text()), args.volume.resolve(),
                        args.volume.stat().st_dev, stats.f_bavail * stats.f_frsize,
                        args.before_free_bytes)
        print(json.dumps(result, indent=2))
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f'{error}\n')


if __name__ == '__main__':
    main()
