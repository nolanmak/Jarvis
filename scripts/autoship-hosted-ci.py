#!/usr/bin/env python3
"""Controller-owned GitHub Actions evidence reader. Never executes repository code.
The controller supplies its embedded CI inventory; candidates cannot choose jobs.
Unknown, incomplete, changed-policy, or stale evidence holds instead of passing.
"""
import base64
import hashlib
import itertools
import json
import re
import subprocess
import sys


def match_path(path, pattern):
    expression = re.escape(pattern).replace(r'\*\*/', '(?:.*/)?').replace(r'\*\*', '.*').replace(r'\*', '[^/]*').replace(r'\?', '[^/]')
    return re.fullmatch(expression, path) is not None


def applicable(trigger, changed):
    trigger = trigger or {}
    if not changed:
        return True
    if 'paths' in trigger:
        for path in changed:
            matched = False
            for pattern in trigger['paths']:
                if match_path(path, pattern.lstrip('!')):
                    matched = not pattern.startswith('!')
            if matched:
                return True
        return False
    return any(not any(match_path(path, p) for p in trigger.get('paths-ignore', [])) for path in changed)


def resolve(value, matrix):
    def replace(match):
        key = match.group(1).strip()
        if not key.startswith('matrix.') or key[7:] not in matrix:
            raise ValueError('unsupported hosted CI expression')
        return str(matrix[key[7:]])
    return re.sub(r'\$\{\{(.*?)\}\}', replace, value)


def on_main(github, prefix, head):
    """`head` is main's tip or a first-parent ancestor of it. Auto-ship reads
    a recent main commit's CI while the tip's own run is still pending; a
    diverged or unrelated commit is never main evidence."""
    branch = github.api(f'{prefix}/branches/main')['commit']['sha']
    if branch == head:
        return True
    compared = github.api(f'{prefix}/compare/{head}...{branch}')
    return compared.get('status') == 'ahead' and compared.get('behind_by') == 0


def expected_jobs(contract, changed, event):
    result = {}
    for path, workflow in contract['workflows'].items():
        if workflow['role'] != 'validation' or event not in workflow['events']:
            continue
        if not applicable(workflow['events'][event], changed):
            continue
        names = set()
        for key, job in workflow['jobs'].items():
            if job.get('if'):
                raise ValueError('unsupported hosted CI job condition')
            matrix = job.get('strategy', {}).get('matrix', {})
            if matrix.get('include'):
                raise ValueError('unsupported hosted CI matrix include')
            keys = [k for k in matrix if k != 'exclude']
            rows = [dict(zip(keys, v)) for v in itertools.product(*(matrix[k] for k in keys))] if keys else [{}]
            for row in rows:
                if any(all(row.get(k) == v for k, v in exc.items()) for exc in matrix.get('exclude', [])):
                    continue
                runner = resolve(job['runs-on'], row)
                if runner.startswith('macos-'):
                    continue
                if not re.fullmatch(r'ubuntu-(latest|[0-9]+\.[0-9]+)(-arm)?', runner):
                    raise ValueError('unsupported hosted CI runner')
                name = resolve(job.get('name', key), row)
                if row and 'name' not in job:
                    name += ' (' + ', '.join(str(row[k]) for k in keys) + ')'
                if name in names:
                    raise ValueError('ambiguous hosted CI job name')
                names.add(name)
        if names:
            result[path] = names
    return result


def job_verdict(expected, jobs):
    latest = {}
    for job in jobs:
        name = job['name']
        if name not in latest or job['id'] > latest[name]['id']:
            latest[name] = job
    failures = []
    pending = False
    for name in sorted(expected):
        job = latest.get(name)
        if job is None or job['status'] != 'completed':
            pending = True
        elif job['conclusion'] in ('failure', 'timed_out', 'startup_failure'):
            failures.append({'name': name, 'job': job['id'], 'url': job.get('html_url', '')})
        elif job['conclusion'] != 'success':
            pending = True
    return ('failed' if failures else 'pending' if pending else 'passed'), failures


def run_matches(run, repository, head, event, path):
    return (isinstance(run.get('id'), int) and run.get('head_sha') == head
            and run.get('event') == event and run.get('path') == path
            and str(run.get('repository', {}).get('full_name', '')).casefold() == repository.casefold())


class GitHub:
    def __init__(self, binary):
        self.binary = binary

    def api(self, endpoint):
        done = subprocess.run([self.binary, 'api', endpoint], capture_output=True, text=True, timeout=30)
        if done.returncode:
            raise ValueError('GitHub evidence unavailable')
        return json.loads(done.stdout)

    def pages(self, endpoint, key=None):
        rows = []
        total = None
        for page in range(1, 101):
            data = self.api(endpoint + ('&' if '?' in endpoint else '?') + f'per_page=100&page={page}')
            batch = data[key] if key else data
            if not isinstance(batch, list):
                raise ValueError('invalid GitHub page')
            rows.extend(batch)
            if key == 'workflow_runs':
                # Filtered Actions searches expose at most 1,000 runs. A
                # partial inventory cannot establish the newest run per policy.
                observed = data.get('total_count')
                if type(observed) is not int or not 0 <= observed <= 1000:
                    raise ValueError('workflow run inventory exceeds bounded search')
                if total is not None and total != observed:
                    raise ValueError('workflow run inventory changed during pagination')
                total = observed
                ids = [row.get('id') for row in rows]
                if (any(type(i) is not int or i <= 0 for i in ids)
                        or len(set(ids)) != len(ids) or len(rows) > total):
                    raise ValueError('workflow run inventory inconsistent')
                if len(rows) == total:
                    return rows
                if len(batch) < 100:
                    raise ValueError('workflow run inventory incomplete')
            elif len(batch) < 100:
                return rows
        raise ValueError('GitHub evidence pagination incomplete')


def evaluate(request, github):
    repository = request['repository']
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository):
        raise ValueError('invalid repository')
    prefix = f'repos/{repository}'
    pr = request.get('pr')
    pull = github.api(f'{prefix}/pulls/{pr}') if pr else None
    event = 'pull_request' if pull else 'push'
    head = pull['head']['sha'] if pull else request['head']
    if not re.fullmatch(r'[a-f0-9]{40}|[a-f0-9]{64}', head):
        raise ValueError('invalid source head')
    if pull and (pull['base']['ref'] != 'main' or pull['state'] != 'open' or pull['base']['repo']['full_name'].casefold() != repository.casefold()):
        raise ValueError('PR source identity unavailable')
    changed = []
    if pull:
        files = github.pages(f'{prefix}/pulls/{pr}/files')
        if len(files) != pull['changed_files'] or len(files) >= 3000:
            raise ValueError('PR changed paths incomplete')
        changed = [f['filename'] for f in files]
        changed.extend(f['previous_filename'] for f in files if 'previous_filename' in f)
    else:
        if not on_main(github, prefix, head):
            raise ValueError('main source moved')
    if not pull:
        # The commit API exposes the diff to the first parent, including the
        # complete PR diff for the protected merge commits used by autoship.
        files = github.pages(f'{prefix}/commits/{head}', 'files')
        if len(files) >= 3000:
            raise ValueError('main changed paths incomplete')
        changed = [f['filename'] for f in files]
        changed.extend(f['previous_filename'] for f in files if 'previous_filename' in f)
    contract = request['contract']
    # Bind the actual executed workflow and helper bytes to controller policy.
    # Changes to CI policy wait for a reviewed controller rollout, rather than
    # allowing model-written checks to certify themselves.
    policy = dict(request['policy_files'])
    for path, workflow in contract['workflows'].items():
        if workflow['role'] == 'validation':
            policy[path] = workflow['sha256']
    # The merge tree runs on PRs, so check both parents for policy changes.
    refs = {head}
    if pull:
        refs.add(pull['base']['sha'])
    for ref in refs:
        for path, expected in policy.items():
            content = github.api(f'{prefix}/contents/{path}?ref={ref}')
            if content.get('encoding') != 'base64' or content.get('type') != 'file':
                raise ValueError('CI policy source unavailable')
            if hashlib.sha256(base64.b64decode(content['content'])).hexdigest() != expected:
                raise ValueError('CI policy changed; reviewed controller rollout required')
    expected = expected_jobs(contract, changed, event)
    # These two always run and cannot disappear with path filters.
    if not {'privacy', 'node-release'}.issubset(set().union(*expected.values()) if expected else set()):
        raise ValueError('mandatory release checks missing from controller policy')
    failures = []
    pending = False
    receipts = []
    inventory = expected if pull else expected_jobs(contract, [], event)
    # One fresh, complete inventory serves every workflow in this observation.
    # Results are never persisted or reused as a passing merge receipt.
    all_runs = github.pages(f'{prefix}/actions/runs?head_sha={head}&event={event}', 'workflow_runs')
    for path, names in sorted(inventory.items()):
        runs = [r for r in all_runs if run_matches(r, repository, head, event, path)
                and (not pull or any(p.get('number') == pr for p in r.get('pull_requests', [])))
                and (pull or r.get('head_branch') == 'main')]
        if not runs:
            # Path-filtered push workflows may legitimately have no run. Every
            # actual run is still checked, including multi-commit pushes whose
            # earlier changes are outside the last commit's diff.
            pending |= path in expected
            continue
        run = max(runs, key=lambda r: r['id'])
        # filter=latest includes the newest incarnation of each job.
        jobs = github.pages(f"{prefix}/actions/runs/{run['id']}/jobs?filter=latest", 'jobs')
        if any(j.get('run_id') != run['id'] or j.get('head_sha') != head
               or not isinstance(j.get('run_attempt'), int)
               or not 1 <= j['run_attempt'] <= run['run_attempt'] for j in jobs):
            raise ValueError('job/run identity mismatch')
        # Workflow status can remain "queued" solely because macOS has no
        # runner. Use actual jobs, while refusing a newly queued attempt that
        # still exposes only the previous attempt's green job records.
        if not any(j['run_attempt'] == run['run_attempt'] for j in jobs):
            pending = True
            continue
        state, failed = job_verdict(names, jobs)
        failures.extend(failed)
        pending |= state == 'pending'
        receipts.append({'workflow': path, 'run': run['id'], 'attempt': run['run_attempt'], 'jobs': sorted(names), 'state': state})
    # Additional branch-protection contexts (including explicitly required macOS)
    # remain mandatory. gh resolves both rulesets and legacy branch protection.
    if pull:
        done = subprocess.run([github.binary, 'pr', 'checks', str(pr), '--repo', repository,
                               '--required', '--json', 'name,state'], capture_output=True, text=True, timeout=30)
        if done.returncode not in (0, 1, 8):
            raise ValueError('required branch checks unavailable')
        checks = json.loads(done.stdout)
        if not checks:
            raise ValueError('required branch checks unavailable')
        pending |= any(c['state'] != 'SUCCESS' for c in checks)
        current = github.api(f'{prefix}/pulls/{pr}')
        if current['head']['sha'] != head or current['base']['sha'] != pull['base']['sha'] or current['state'] != 'open':
            raise ValueError('PR moved during CI observation')
    elif not on_main(github, prefix, head):
        raise ValueError('main moved during CI observation')
    return {'state': 'failed' if failures else 'pending' if pending else 'passed',
            'head': head, 'failures': failures, 'receipts': receipts}


def main():
    try:
        request = json.load(sys.stdin)
        result = evaluate(request, GitHub(request['gh']))
    except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired) as error:
        result = {'state': 'unknown', 'reason': str(error)}
    print(json.dumps(result, separators=(',', ':')))


if __name__ == '__main__':
    main()
