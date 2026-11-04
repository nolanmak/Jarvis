#!/usr/bin/env python3
"""Targeted PR check: test only the crates a change can affect, then require
every other applicable Linux CI job for the same head.

  plan  changed files -> mode + affected crates (JSON written to --out)
  test  `cargo test` for the affected crates; no-op unless mode is `targeted`
  wait  poll GitHub for every applicable Linux validation job on this PR head
        (scripts/ci-contract.json + workflow path filters, base and head
        contracts) and fail if any fails, is cancelled/skipped, or never runs

Modes:
  targeted  every changed path is a crate source/data file: inside a
            workspace crate, not its Cargo.toml, a build script, or a
            Python/shell file (those have non-cargo CI commands). Affected =
            owning crates plus every crate that depends on them (Cargo
            dependencies of any kind, or files it compiles in), plus crates
            whose tests read their files (named paths, and the
            WORKSPACE_SCANNERS whose tests walk every crate).
  full      anything else: workflows, lockfile, manifests, build scripts,
            toolchain, scripts/, docs/, schema/ (crates embed files from all
            of these), unknown paths. rust.yml and platform.yml trigger on
            exactly these paths (FULL_SUITE_PATHS) and `wait` requires them.
  none      no changed files.

Fail-safe: a path the planner cannot place in a crate is `full`.
"""
import argparse, importlib.util, json, os, re, subprocess, time, urllib.error, urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SELF_WORKFLOW = '.github/workflows/targeted.yml'
FULL_SUITE_WORKFLOWS = ['.github/workflows/rust.yml', '.github/workflows/platform.yml']
# The pull_request `paths` of rust.yml and platform.yml (before the contract
# inputs ci-contract-sync.py appends). ci_targeted_test.py checks that the
# planner's `full` rule and these filters agree for every tracked file.
FULL_SUITE_PATHS = ['**', '!crates/**', 'crates/*/Cargo.toml', 'crates/**/build.rs', 'crates/**/*.py', 'crates/**/*.sh']
FULL_SUFFIXES = ('/build.rs', '.py', '.sh')
# Crates whose tests walk every crate's sources (capability inventory,
# notification producer registry, Slack parity matrix, autonomy card-site
# exemptions). Any crate change
# re-runs them. ci_targeted_test.py fails if another crate starts doing so.
WORKSPACE_SCANNERS = {'augmentagent-channel-core', 'augmentagent-cli', 'augmentagent-channel-slack', 'augmentagent-autonomy'}
# Literal references from one crate's sources to another crate's files.
FILE_REFERENCE = re.compile(r'(?:include_(?:str|bytes)!|include!)\(\s*"([^"]+)"|#\[path\s*=\s*"([^"]+)"\]|"(crates/augmentagent-[a-z0-9-]+)')
# The environment the full suites give these tests.
TEST_ENV = {'AUGMENTAGENT_GH_DISABLE': '1', 'AUGMENTAGENT_PHONE_ENABLED': '0',
            'JARVIS_TEST_VM_CONFIG': '', 'JARVIS_TEST_COMPUTE_PIP': ''}


def contract_runner():
    spec = importlib.util.spec_from_file_location('ci_contract', ROOT / 'scripts/ci-contract.py')
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module


def workspace(root):
    """name -> {'dir': repo-relative dir, 'deps': workspace crates it depends on or reads}."""
    meta = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--format-version', '1', '--no-deps', '--locked', '--offline'],
        cwd=root, text=True, env={**os.environ, 'CARGO_TERM_COLOR': 'never'}))
    root = Path(meta['workspace_root']).resolve()
    members = set(meta['workspace_members'])
    crates = {p['name']: {'dir': str(Path(p['manifest_path']).resolve().parent.relative_to(root)), 'deps': set(), 'reads': set()}
              for p in meta['packages'] if p['id'] in members}
    for package in meta['packages']:
        if package['name'] in crates:
            crates[package['name']]['deps'] = {d['name'] for d in package['dependencies'] if d.get('path') and d['name'] in crates}
    by_dir = {row['dir']: name for name, row in crates.items()}
    for name, row in crates.items():
        for source in (root / row['dir']).rglob('*.rs'):
            for include, path_attr, crate_path in FILE_REFERENCE.findall(source.read_text(errors='replace')):
                relative = include or path_attr
                if relative:
                    target = (source.parent / relative).resolve()
                    parts = target.relative_to(root).parts if target.is_relative_to(root) else ()
                    crate_path = '/'.join(parts[:2]) if parts[:1] == ('crates',) else ''
                other = by_dir.get(crate_path)
                if other and other != name:
                    # Compiled-in files propagate like a dependency; a named
                    # path is only read by that crate's own tests.
                    row['deps' if relative else 'reads'].add(other)
    for scanner in WORKSPACE_SCANNERS & set(crates):
        crates[scanner]['reads'] |= set(crates) - {scanner}
    return crates


def dependents(crates, seeds):
    """Seeds, their transitive dependents, and every crate that reads one of
    those (reads don't propagate further: only the reader's tests look)."""
    reverse = {name: set() for name in crates}
    for name, row in crates.items():
        for dep in row['deps']:
            reverse[dep].add(name)
    result, stack = set(seeds), list(seeds)
    while stack:
        for parent in reverse[stack.pop()]:
            if parent not in result:
                result.add(parent); stack.append(parent)
    return result | {name for name, row in crates.items() if row.get('reads', set()) & result}


def owner(path, crates):
    """The crate a path belongs to, or None when the change must run in full."""
    if not path.startswith('crates/') or path.endswith(FULL_SUFFIXES):
        return None
    for name, row in crates.items():
        if path.startswith(row['dir'] + '/'):
            return None if path == row['dir'] + '/Cargo.toml' else name
    return None


def classify(changed, crates):
    owners, full = set(), []
    for path in changed:
        name = owner(path, crates)
        if name is None:
            full.append(path)
        else:
            owners.add(name)
    if full:
        return {'mode': 'full', 'reasons': sorted(full)[:20], 'crates': [], 'affected': []}
    if not owners:
        return {'mode': 'none', 'reasons': ['no changed files'], 'crates': [], 'affected': []}
    return {'mode': 'targeted', 'reasons': [], 'crates': sorted(owners), 'affected': sorted(dependents(crates, owners))}


def changed_files(root, base, head):
    merge_base = subprocess.check_output(['git', 'merge-base', base, head], cwd=root, text=True).strip()
    out = subprocess.check_output(['git', 'diff', '-z', '--name-only', '--no-renames', merge_base, head], cwd=root, text=True)
    return [path for path in out.split('\0') if path]


def test_commands(affected):
    """cargo invocations for the affected crates, as the full suites run them."""
    crates = [c for c in affected if c != 'augmentagent-cli']
    commands = []
    if crates:
        commands.append(['cargo', 'test', '--locked', '--no-fail-fast'] + [a for c in crates for a in ('-p', c)])
    if 'augmentagent-cli' in affected:
        # The suites run the CLI binary's tests single-threaded and its
        # integration tests normally.
        commands.append(['cargo', 'test', '--locked', '--no-fail-fast', '-p', 'augmentagent-cli', '--bins', '--', '--test-threads=1'])
        commands.append(['cargo', 'test', '--locked', '--no-fail-fast', '-p', 'augmentagent-cli', '--test', '*'])
    return commands


def expected_jobs(contract, changed, ci):
    """workflow path -> Linux job display names that run for this change."""
    result = {}
    for path, workflow in contract['workflows'].items():
        if workflow.get('role') != 'validation' or path == SELF_WORKFLOW or 'pull_request' not in workflow.get('events', {}):
            continue
        trigger = workflow['events']['pull_request'] or {}
        if changed:
            if not ci.applicable(workflow['events'], changed):
                continue
        elif trigger.get('paths') or trigger.get('paths-ignore'):
            continue  # GitHub starts no path-filtered workflow for an empty diff
        names = set()
        for key, job in workflow['jobs'].items():
            for row in ci.matrices(job):
                context = {'matrix.' + k: v for k, v in row.items()}
                if not ci.resolve(job['runs-on'], context).startswith('ubuntu-'):
                    continue  # macOS runs post-merge (macos.yml) and is informational
                name = ci.resolve(job.get('name', key), context)
                if row and 'name' not in job:
                    name += ' (' + ', '.join(str(v) for v in row.values()) + ')'
                names.add(name)
        if names:
            result[path] = names
    return result


def required_jobs(contracts, plan, ci):
    """Union over the base and head contracts, so a PR cannot drop a check by
    editing the inventory it is judged by."""
    expected = {}
    for contract in contracts:
        for path, names in expected_jobs(contract, plan['changed'], ci).items():
            expected.setdefault(path, set()).update(names)
        if plan['mode'] == 'full':
            # Fail closed: a full-mode change needs the full suites even if a
            # path filter did not start them.
            every = expected_jobs(contract, ['Cargo.lock'], ci)
            for path in FULL_SUITE_WORKFLOWS:
                if path in every:
                    expected.setdefault(path, set()).update(every[path])
    return expected


def latest_runs(expected, runs, pr_number):
    latest = {}
    for run in runs:
        path = run.get('path', '').split('@')[0]
        if run.get('event') != 'pull_request' or path not in expected:
            continue
        numbers = [pr['number'] for pr in run.get('pull_requests') or []]
        if pr_number and numbers and pr_number not in numbers:
            continue  # same head pushed to another PR
        if path not in latest or (run['run_number'], run['run_attempt']) > (latest[path]['run_number'], latest[path]['run_attempt']):
            latest[path] = run
    return latest


def verdict(expected, latest, jobs_by_run):
    """('pass'|'pending'|'fail', details) from the latest run per workflow."""
    pending, failed = [], []
    for path, names in sorted(expected.items()):
        run = latest.get(path)
        if run is None:
            pending.extend(path + ': ' + name + ' (no run yet)' for name in sorted(names)); continue
        if run['id'] not in jobs_by_run:
            pending.extend(path + ': ' + name for name in sorted(names)); continue
        jobs = {job['name']: job for job in jobs_by_run[run['id']]}
        for name in sorted(names):
            job = jobs.get(name)
            if job is None:
                failed.append(path + ': ' + name + ' -> missing from a finished run')
            elif job['status'] != 'completed':
                pending.append(path + ': ' + name)
            elif job['conclusion'] != 'success':
                failed.append(path + ': ' + name + ' -> ' + str(job['conclusion']))
    if failed:
        return 'fail', failed
    return ('pending', pending) if pending else ('pass', [])


def github(url, token):
    request = urllib.request.Request(url, headers={'Authorization': 'Bearer ' + token, 'Accept': 'application/vnd.github+json'})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def wait(args, plan):
    if not os.environ.get('GITHUB_RUN_ID'):
        print('not a GitHub Actions run: the local controller plans and runs the applicable workflows itself')
        return 0
    token, repo = os.environ.get('GH_TOKEN'), os.environ.get('GITHUB_REPOSITORY')
    if not token or not repo:
        print('GH_TOKEN and GITHUB_REPOSITORY are required in GitHub Actions'); return 1
    ci = contract_runner()
    contracts = [json.loads((ROOT / 'scripts/ci-contract.json').read_text())]
    try:
        contracts.append(json.loads(subprocess.check_output(['git', 'show', args.base + ':scripts/ci-contract.json'], cwd=ROOT, text=True)))
    except (subprocess.CalledProcessError, ValueError):
        pass
    expected = required_jobs(contracts, plan, ci)
    print('requiring: ' + json.dumps({k: sorted(v) for k, v in sorted(expected.items())}, indent=1), flush=True)
    pr_number = int(os.environ['PR_NUMBER']) if os.environ.get('PR_NUMBER') else None
    api = 'https://api.github.com/repos/' + repo
    started, deadline, finished = time.time(), time.time() + args.timeout_minutes * 60, {}
    while True:
        try:
            runs = github(api + '/actions/runs?event=pull_request&per_page=100&head_sha=' + args.head, token)['workflow_runs']
            latest = latest_runs(expected, runs, pr_number)
            for run in latest.values():
                key = (run['id'], run['run_attempt'])
                if run['status'] == 'completed' and key not in finished:
                    finished[key] = github(api + '/actions/runs/' + str(run['id']) + '/jobs?filter=latest&per_page=100', token)['jobs']
            state, details = verdict(expected, latest, {run['id']: finished[(run['id'], run['run_attempt'])]
                                                         for run in latest.values() if (run['id'], run['run_attempt']) in finished})
            missing = [path for path in expected if path not in latest]
            if state == 'pending' and missing and latest and time.time() - started > args.start_grace_minutes * 60:
                state, details = 'fail', [path + ': no run started for this head (path filter disagreed with the planner, or the 300-file diff cap); push again or run it by hand' for path in missing]
        except (urllib.error.URLError, OSError, ValueError, KeyError) as error:
            state, details = 'pending', ['GitHub API read failed: ' + str(error)]
        if state != 'pending':
            print(state + ('' if not details else ':\n  ' + '\n  '.join(details)))
            return 0 if state == 'pass' else 1
        if time.time() > deadline:
            print('timed out waiting for:\n  ' + '\n  '.join(details)); return 1
        print('waiting for ' + str(len(details)) + ' job(s): ' + ', '.join(details[:4]), flush=True)
        time.sleep(args.interval)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='op', required=True)
    p = sub.add_parser('plan'); p.add_argument('--base', required=True); p.add_argument('--head', required=True); p.add_argument('--out', type=Path, required=True)
    p = sub.add_parser('test'); p.add_argument('--plan', type=Path, required=True)
    p = sub.add_parser('wait'); p.add_argument('--plan', type=Path, required=True); p.add_argument('--base', required=True); p.add_argument('--head', required=True)
    p.add_argument('--timeout-minutes', type=int, default=120); p.add_argument('--interval', type=int, default=90)
    p.add_argument('--start-grace-minutes', type=int, default=30)
    args = parser.parse_args()
    if args.op == 'plan':
        changed = changed_files(ROOT, args.base, args.head)
        plan = classify(changed, workspace(ROOT)); plan['changed'] = changed
        args.out.write_text(json.dumps(plan, indent=2) + '\n')
        print(json.dumps({k: plan[k] for k in ('mode', 'crates', 'affected', 'reasons')}, indent=2))
        return 0
    plan = json.loads(args.plan.read_text())
    if args.op == 'test':
        if plan['mode'] != 'targeted':
            print('mode ' + plan['mode'] + ': no targeted cargo tests (the full suites cover a full-mode change)')
            return 0
        status = 0
        for command in test_commands(plan['affected']):
            print('+ ' + ' '.join(command), flush=True)
            status |= subprocess.call(command, cwd=ROOT, env={**os.environ, **TEST_ENV})
        return 1 if status else 0
    return wait(args, plan)


if __name__ == '__main__':
    raise SystemExit(main())
