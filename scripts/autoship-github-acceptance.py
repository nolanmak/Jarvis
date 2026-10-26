#!/usr/bin/env python3
"""Provision ONLY synthetic GitHub data, then run the production merge adapter.
No execution without --execute. Requires an already built managed CLI test binary.
The repository is retained with its evidence; no production settings are touched.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

MARKER = 'Synthetic autoship acceptance fixture; no production source or credentials.'
TEST = 'self_improve::publication::live_probe::disposable_github_merge_boundary'
WORKFLOW = '''name: Synthetic boundary checks
on: [push, pull_request]
permissions:
  contents: read
jobs:
  privacy:
    runs-on: ubuntu-latest
    steps:
      - run: echo synthetic-privacy-pass
  node-release:
    runs-on: ubuntu-latest
    steps:
      - run: echo synthetic-node-pass
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--test-binary', required=True, type=Path)
    args = parser.parse_args()
    if not args.execute:
        parser.error('--execute is required; this command creates a synthetic PUBLIC repository')
    binary = args.test_binary.resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix='autoship-github-acceptance-'))
    repo_root = root / 'repository'

    def call(*cmd, cwd=None, env=None, stdin=None):
        result = subprocess.run(cmd, cwd=cwd, env=env, input=stdin, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=600)
        if result.returncode:
            raise RuntimeError(f'{cmd[0]} failed: {result.stderr[-3000:]}')
        return result.stdout.strip()

    def gh(*cmd, **kwargs):
        return call('gh', *cmd, **kwargs)

    def git(*cmd):
        return call('git', *cmd, cwd=repo_root)

    owner = gh('api', 'user', '--jq', '.login')
    repository = f'{owner}/autoship-acceptance-{uuid.uuid4().hex[:16]}'
    (root / 'repository.json').write_text(json.dumps({'repository': repository, 'marker': MARKER}))
    gh('repo', 'create', repository, '--public', '--description', MARKER)
    call('git', 'clone', f'https://github.com/{repository}.git', str(repo_root))
    git('config', 'user.name', 'Synthetic Autoship Probe')
    git('config', 'user.email', 'probe@example.invalid')
    git('checkout', '-b', 'main')
    (repo_root / '.github/workflows').mkdir(parents=True)
    (repo_root / '.github/workflows/boundary.yml').write_text(WORKFLOW)
    (repo_root / 'README.md').write_text(MARKER + '\n')
    git('add', '.')
    git('commit', '-m', 'Synthetic boundary fixture')
    git('push', '-u', 'origin', 'main')
    gh('repo', 'edit', repository, '--default-branch', 'main', '--enable-squash-merge')
    seed = git('rev-parse', 'HEAD')
    policy = {'required_status_checks': {'strict': True, 'checks': [
        {'context': 'privacy', 'app_id': 15368}, {'context': 'node-release', 'app_id': 15368}]},
        'enforce_admins': True, 'required_pull_request_reviews': None,
        'restrictions': None, 'allow_force_pushes': False, 'allow_deletions': False}
    gh('api', '--method', 'PUT', f'repos/{repository}/branches/main/protection',
       '--input', '-', stdin=json.dumps(policy))
    protection = gh('api', f'repos/{repository}/branches/main/protection')
    (root / 'protection.json').write_text(protection)
    actual = json.loads(protection)
    assert actual['required_status_checks']['strict'] and actual['enforce_admins']['enabled']

    def create(branch, base='origin/main'):
        git('fetch', 'origin')
        git('checkout', '-b', branch, base)
        (repo_root / f'{branch}.txt').write_text('synthetic\n')
        git('add', f'{branch}.txt')
        git('commit', '-m', f'Synthetic {branch}')
        git('push', '-u', 'origin', branch)
        url = gh('pr', 'create', '--repo', repository, '--head', branch, '--base', 'main',
                 '--title', f'Synthetic {branch}', '--body', MARKER)
        pr = url.rsplit('/', 1)[-1]
        wait(pr)
        return pr, git('rev-parse', 'HEAD')

    def wait(pr):
        # Do not convert missing/failed checks into green. A temporary "no checks"
        # API response is retried, with a bounded dispatch-only polling window.
        import time
        for _ in range(60):
            raw = gh('pr', 'view', pr, '--repo', repository, '--json', 'statusCheckRollup')
            checks = json.loads(raw)['statusCheckRollup']
            names = {c.get('name') for c in checks if c.get('conclusion') == 'SUCCESS'}
            if {'privacy', 'node-release'} <= names:
                return
            if any(c.get('conclusion') in {'FAILURE', 'CANCELLED', 'TIMED_OUT'} for c in checks):
                raise RuntimeError('synthetic checks failed')
            time.sleep(5)
        raise RuntimeError('synthetic checks did not complete within five minutes')

    def probe(mode, pr, head, **extra):
        env = dict(os.environ, AUTOSHIP_GITHUB_PROBE_REPOSITORY=repository,
                   AUTOSHIP_GITHUB_PROBE_ROOT=str(repo_root), AUTOSHIP_GITHUB_PROBE_MODE=mode,
                   AUTOSHIP_GITHUB_PROBE_PR=pr, AUTOSHIP_GITHUB_PROBE_HEAD=head, **extra)
        output = call(str(binary), '--exact', TEST, '--ignored', '--nocapture', env=env)
        (root / f'{pr}-{mode}.log').write_text(output)
        if '1 passed' not in output:
            raise RuntimeError('test selector did not execute exactly one passing probe')

    a, ah = create('candidate-a', seed)
    b, bh = create('advance-main', seed)
    probe('accepted', b, bh)
    probe('stale', a, ah)
    git('fetch', 'origin')
    git('checkout', 'candidate-a')
    git('merge', '--no-edit', 'origin/main')
    git('push')
    wait(a)
    ah = git('rev-parse', 'HEAD')
    (repo_root / 'race.txt').write_text('synthetic concurrent candidate update\n')
    git('add', 'race.txt')
    git('commit', '-m', 'Synthetic head race')
    race = git('rev-parse', 'HEAD')
    git('push', 'origin', 'HEAD:refs/heads/race-object')
    probe('head-race', a, ah, AUTOSHIP_GITHUB_PROBE_BRANCH='candidate-a', AUTOSHIP_GITHUB_PROBE_RACE_HEAD=race)
    wait(a)
    probe('accepted', a, race)
    lost, lh = create('lost-response')
    probe('lost', lost, lh)
    manual, mh = create('external-merge')
    gh('pr', 'merge', manual, '--repo', repository, '--squash', '--match-head-commit', mh)
    probe('manual', manual, mh)
    (root / 'result.json').write_text(json.dumps({'repository': repository, 'passed': True,
        'assertions': ['strict-base-rejection', 'head-race-rejection', 'own-receipt',
                       'lost-response-no-replay-no-attribution', 'external-merge-no-attribution']}))
    print(f'PASS: {repository}; retained evidence: {root}')


if __name__ == '__main__':
    main()
