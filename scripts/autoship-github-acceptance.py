#!/usr/bin/env python3
"""Provision ONLY synthetic GitHub data, then run the production merge adapter.
No execution without --execute. Requires an already built managed CLI test binary.
The repository is retained with its evidence; no production settings are touched.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

MARKER = 'Synthetic autoship acceptance fixture; no production source or credentials.'
TEST = 'self_improve::publication::live_probe::disposable_github_merge_boundary'


def artifact_identity(stream):
    stream.seek(0)
    return {'sha256': hashlib.file_digest(stream, 'sha256').hexdigest(),
            'bytes': os.fstat(stream.fileno()).st_size}


def validate_checkpoint(checkpoint, artifact, root):
    if checkpoint.get('version') != 2 or 'artifact' not in checkpoint:
        raise ValueError('checkpoint-artifact-unbound: retain historical evidence; it cannot prove this build')
    if checkpoint['artifact'] != artifact:
        raise ValueError('checkpoint-artifact-mismatch: select the original artifact or a fresh fixture')
    for stage in checkpoint.get('done', {}).values():
        if (not isinstance(stage, dict) or stage.get('artifact') != artifact
                or not isinstance(stage.get('log'), str)
                or Path(stage['log']).name != stage['log']
                or not isinstance(stage.get('log_sha256'), str)):
            raise ValueError('checkpoint-stage-unbound: historical logs cannot be upgraded to current proof')
        log = root / stage['log']
        if not log.is_file() or hashlib.sha256(log.read_bytes()).hexdigest() != stage['log_sha256']:
            raise ValueError('checkpoint-stage-log-mismatch')


def run_probe(stream, artifact, env):
    if artifact_identity(stream) != artifact:
        raise ValueError('artifact-changed-before-dispatch')
    return subprocess.run([f'/proc/self/fd/{stream.fileno()}', '--exact', TEST, '--ignored', '--nocapture'],
                          pass_fds=(stream.fileno(),), env=env,
                          text=True, capture_output=True, timeout=600)


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
    parser.add_argument('--resume-root', type=Path, help='resume a retained synthetic fixture; never creates another repository')
    parser.add_argument('--test-binary', required=True, type=Path)
    args = parser.parse_args()
    if not args.execute:
        parser.error('--execute is required; this command creates a synthetic PUBLIC repository')
    binary = args.test_binary.resolve(strict=True)
    # Hold the exact executable inode across GitHub waits and every dispatch.
    # A concurrent managed rebuild may replace the pathname; it must never
    # change which binary runs under this checkpoint's evidence identity.
    if not Path('/proc/self/fd').is_dir():
        raise ValueError('artifact-pinning-requires-linux')
    artifact_file = binary.open('rb')
    artifact = artifact_identity(artifact_file)
    root = args.resume_root.resolve(strict=True) if args.resume_root else Path(tempfile.mkdtemp(prefix='autoship-github-acceptance-'))
    repo_root = root / 'repository'
    print(f'Fixture evidence: {root}', flush=True)

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

    checkpoint_path = root / 'checkpoint.json'
    if args.resume_root and not checkpoint_path.exists():
        raise ValueError('checkpoint-artifact-unbound: resume requires a bound checkpoint')
    checkpoint = (json.loads(checkpoint_path.read_text()) if checkpoint_path.exists() else
                  {'version': 2, 'artifact': artifact, 'prs': {}, 'done': {}})
    # Refuse before even reading remote state, let alone creating or merging PRs.
    validate_checkpoint(checkpoint, artifact, root)

    def save():
        temporary = root / 'checkpoint.tmp'
        with temporary.open('w') as stream:
            json.dump(checkpoint, stream)
            stream.flush()
            os.fsync(stream.fileno())
        temporary.replace(checkpoint_path)
        descriptor = os.open(root, os.O_RDONLY)
        os.fsync(descriptor)
        os.close(descriptor)

    if not args.resume_root:
        save()
    if args.resume_root:
        identity = json.loads((root / 'repository.json').read_text())
        repository = identity['repository']
        assert identity['marker'] == MARKER and repository.split('/')[1].startswith('autoship-acceptance-')
        metadata = json.loads(gh('api', f'repos/{repository}'))
        assert metadata['description'] == MARKER and not metadata['archived']
        remote = git('remote', 'get-url', 'origin')
        assert remote in {f'https://github.com/{repository}.git', f'git@github.com:{repository}.git'}
        seed = git('rev-list', '--max-parents=0', 'origin/main')
        assert len(seed.splitlines()) == 1
    else:
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
    checkpoint.setdefault('seed', seed)
    assert checkpoint['seed'] == seed
    save()
    print(f'Synthetic repository: {repository}', flush=True)
    policy = {'required_status_checks': {'strict': True, 'checks': [
        {'context': 'privacy', 'app_id': 15368}, {'context': 'node-release', 'app_id': 15368}]},
        'enforce_admins': True, 'required_pull_request_reviews': None,
        'restrictions': None, 'allow_force_pushes': False, 'allow_deletions': False}
    if not args.resume_root:
        gh('api', '--method', 'PUT', f'repos/{repository}/branches/main/protection',
           '--input', '-', stdin=json.dumps(policy))
    protection = gh('api', f'repos/{repository}/branches/main/protection')
    (root / 'protection.json').write_text(protection)
    actual = json.loads(protection)
    assert actual['required_status_checks']['strict'] and actual['enforce_admins']['enabled']

    def create(branch, base='origin/main'):
        if branch in checkpoint['prs']:
            item = checkpoint['prs'][branch]
            return item['pr'], item['initial_head']
        existing = json.loads(gh('pr', 'list', '--repo', repository, '--head', branch,
                                '--state', 'all', '--json', 'number,headRefOid,headRefName,isCrossRepository'))
        assert len(existing) <= 1, 'ambiguous synthetic PR identity'
        if existing:
            item = existing[0]
            assert item['headRefName'] == branch and not item['isCrossRepository']
            pr, head = str(item['number']), item['headRefOid']
        else:
            git('fetch', 'origin')
            branches = git('branch', '--list', branch)
            if branches:
                git('checkout', branch)
            else:
                git('checkout', '-b', branch, base)
                (repo_root / f'{branch}.txt').write_text('synthetic\n')
                git('add', f'{branch}.txt')
                git('commit', '-m', f'Synthetic {branch}')
            git('push', '-u', 'origin', branch)
            url = gh('pr', 'create', '--repo', repository, '--head', branch, '--base', 'main',
                     '--title', f'Synthetic {branch}', '--body', MARKER)
            pr, head = url.rsplit('/', 1)[-1], git('rev-parse', 'HEAD')
            wait(pr)
        checkpoint['prs'][branch] = {'pr': pr, 'initial_head': head}
        save()
        return pr, head

    def wait(pr):
        # Do not convert missing/failed checks into green. A temporary "no checks"
        # API response is retried, with a bounded dispatch-only polling window.
        import time
        for _ in range(60):
            raw = gh('pr', 'view', pr, '--repo', repository, '--json', 'statusCheckRollup')
            checks = json.loads(raw)['statusCheckRollup']
            names = {c.get('name') for c in checks if c.get('conclusion') == 'SUCCESS'}
            # push and pull_request can report the same context twice. One
            # successful run must not hide another required run still queued.
            required = [c for c in checks if c.get('name') in {'privacy', 'node-release'}]
            if {'privacy', 'node-release'} <= names and all(
                    c.get('status') == 'COMPLETED' and c.get('conclusion') == 'SUCCESS'
                    for c in required):
                return
            if any(c.get('conclusion') in {'FAILURE', 'CANCELLED', 'TIMED_OUT'} for c in checks):
                raise RuntimeError('synthetic checks failed')
            time.sleep(5)
        raise RuntimeError('synthetic checks did not complete within five minutes')

    def probe(mode, pr, head, **extra):
        key = f'{pr}-{mode}'
        if artifact_identity(artifact_file) != artifact:
            raise ValueError('artifact-changed-before-dispatch')
        validate_checkpoint(checkpoint, artifact, root)
        if key in checkpoint['done']:
            assert checkpoint['done'][key]['head'] == head
            log = root / checkpoint['done'][key]['log']
            assert log.exists() and '1 passed' in log.read_text()
            print(f'Reusing verified stage {key}', flush=True)
            return
        print(f'Checking {mode}: PR #{pr}', flush=True)
        env = dict(os.environ, AUTOSHIP_GITHUB_PROBE_REPOSITORY=repository,
                   AUTOSHIP_GITHUB_PROBE_ROOT=str(repo_root), AUTOSHIP_GITHUB_PROBE_MODE=mode,
                   AUTOSHIP_GITHUB_PROBE_PR=pr, AUTOSHIP_GITHUB_PROBE_HEAD=head, **extra)
        log = root / f'{key}-attempt-{uuid.uuid4().hex[:12]}.log'
        result = run_probe(artifact_file, artifact, env)
        output = result.stdout + '\n' + result.stderr
        log.write_text(output)
        actual = artifact_identity(artifact_file)
        checkpoint.setdefault('attempts', []).append({'stage': key, 'head': head, 'log': log.name,
            'exit': result.returncode, 'artifact': artifact, 'artifact_after': actual})
        save()
        if actual != artifact:
            raise ValueError('artifact-changed-during-dispatch: no successful stage recorded')
        if result.returncode or '1 passed' not in output:
            raise RuntimeError(f'probe {key} failed; retained exact error evidence at {log}')
        checkpoint['done'][key] = {'head': head, 'log': log.name, 'artifact': artifact,
                                  'log_sha256': hashlib.sha256(log.read_bytes()).hexdigest()}
        save()

    a, ah = create('candidate-a', seed)
    b, bh = create('advance-main', seed)
    probe('accepted', b, bh)
    probe('stale', a, ah)
    # An interrupted old-head race may already have advanced its ref. Preserve
    # that experiment and use a new synthetic PR; never rewind the prior ref.
    active = checkpoint.get('active_race')
    if active and active.get('pr') and active.get('before'):
        key = f"{active['pr']}-head-race"
        observed = gh('pr', 'view', active['pr'], '--repo', repository, '--json', 'headRefOid', '--jq', '.headRefOid')
        if key not in checkpoint['done'] and observed != active['before']:
            checkpoint.setdefault('retired_races', []).append(active)
            checkpoint.pop('active_race')
            save()
    if 'active_race' not in checkpoint:
        checkpoint['active_race'] = {'branch': f"head-race-{uuid.uuid4().hex[:8]}"}
        save()
    active = checkpoint['active_race']
    race_pr, race_before = create(active['branch'])
    active.update(pr=race_pr, before=race_before)
    save()
    if not active.get('after'):
        git('checkout', active['branch'])
        path = repo_root / f"{active['branch']}-update.txt"
        if not path.exists():
            path.write_text('synthetic concurrent candidate update\n')
            git('add', path.name)
            git('commit', '-m', 'Synthetic head race')
        race = git('rev-parse', 'HEAD')
        assert race != race_before
        git('push', 'origin', f"HEAD:refs/heads/{active['branch']}-object")
        active['after'] = race
        save()
    race = active['after']
    probe('head-race', race_pr, race_before, AUTOSHIP_GITHUB_PROBE_BRANCH=active['branch'], AUTOSHIP_GITHUB_PROBE_RACE_HEAD=race)
    wait(race_pr)
    probe('accepted', race_pr, race)
    lost, lh = create('lost-response')
    probe('lost', lost, lh)
    manual, mh = create('external-merge')
    if gh('pr', 'view', manual, '--repo', repository, '--json', 'state', '--jq', '.state') != 'MERGED':
        gh('pr', 'merge', manual, '--repo', repository, '--squash', '--match-head-commit', mh)
    probe('manual', manual, mh)
    concurrent = checkpoint.get('active_manual_race')
    if concurrent and concurrent.get('pr'):
        key = f"{concurrent['pr']}-manual-race"
        observed = gh('pr', 'view', concurrent['pr'], '--repo', repository, '--json', 'state', '--jq', '.state')
        if key not in checkpoint['done'] and observed == 'MERGED':
            checkpoint.setdefault('retired_manual_races', []).append(concurrent)
            checkpoint.pop('active_manual_race')
            save()
    if 'active_manual_race' not in checkpoint:
        checkpoint['active_manual_race'] = {'branch': f"external-race-{uuid.uuid4().hex[:8]}"}
        save()
    concurrent = checkpoint['active_manual_race']
    concurrent_pr, concurrent_head = create(concurrent['branch'])
    concurrent.update(pr=concurrent_pr, head=concurrent_head)
    save()
    probe('manual-race', concurrent_pr, concurrent_head)
    (root / 'result.json').write_text(json.dumps({'version': 2, 'artifact': artifact, 'repository': repository, 'passed': True,
        'assertions': ['strict-base-rejection', 'head-race-rejection', 'own-receipt',
                       'lost-response-no-replay-no-attribution', 'external-merge-no-attribution',
                       'concurrent-external-merge-no-attribution']}))
    print(f'PASS: {repository}; retained evidence: {root}')


if __name__ == '__main__':
    main()
