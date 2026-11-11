#!/usr/bin/env python3
"""scripts/ci-targeted.py: planner rules, agreement with the full-suite path
filters, cross-crate file references, and the wait verdict. No CI runs."""
import importlib.util, json, re, subprocess, unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module
targeted = load('ci_targeted', 'scripts/ci-targeted.py')
ci = load('ci_contract_for_targeted', 'scripts/ci-contract.py')
CONTRACT = json.loads((ROOT / 'scripts/ci-contract.json').read_text())
CONTRACT_INPUTS = ['scripts/ci-contract.py', 'scripts/ci-contract.json', 'scripts/ci-contract-sync.py', 'scripts/tests/ci_contract_test.py', 'scripts/ci-pinned-tools.py', 'scripts/tests/ci_pinned_tools_test.py']
FIXTURE = {
    'core': {'dir': 'crates/core', 'deps': set()},
    'store': {'dir': 'crates/store', 'deps': {'core'}},
    'cli': {'dir': 'crates/cli', 'deps': {'store'}},
    'other': {'dir': 'crates/other', 'deps': set()},
}

class Planner(unittest.TestCase):
    def test_crate_changes_select_owner_and_transitive_dependents(self):
        plan = targeted.classify(['crates/core/src/lib.rs', 'crates/other/tests/x.rs'], FIXTURE)
        self.assertEqual(plan['mode'], 'targeted')
        self.assertEqual(plan['crates'], ['core', 'other'])
        self.assertEqual(plan['affected'], ['cli', 'core', 'other', 'store'])
        self.assertEqual(targeted.classify(['crates/cli/src/main.rs'], FIXTURE)['affected'], ['cli'])

    def test_anything_outside_crate_sources_runs_full(self):
        for path in ['Cargo.lock', 'Cargo.toml', 'crates/core/Cargo.toml', 'crates/core/build.rs',
                     'crates/core/python/convert.py', 'crates/core/tests/run.sh',
                     'crates/core/sub/build.rs', 'crates/unknown/src/lib.rs', 'crates/README.md',
                     '.github/workflows/rust.yml', 'scripts/ci-contract.json', 'docs/x.md',
                     'schema/wiki-ask.md', 'rust-toolchain.toml', '.cargo/config.toml', 'src/app.ts']:
            plan = targeted.classify(['crates/core/src/lib.rs', path], FIXTURE)
            self.assertEqual(plan['mode'], 'full', path)
        self.assertEqual(targeted.classify([], FIXTURE)['mode'], 'none')

class AgreesWithWorkflows(unittest.TestCase):
    def test_full_suites_trigger_on_exactly_the_full_mode_paths(self):
        for name in ['rust.yml', 'platform.yml']:
            paths = CONTRACT['workflows']['.github/workflows/' + name]['events']['pull_request']['paths']
            self.assertEqual([p for p in paths if p not in CONTRACT_INPUTS], targeted.FULL_SUITE_PATHS, name)
        self.assertEqual(CONTRACT['workflows'][targeted.SELF_WORKFLOW]['events'], {'pull_request': None})

    def test_every_tracked_file_is_full_exactly_when_the_full_suites_run(self):
        crates = targeted.workspace(ROOT)
        events = CONTRACT['workflows']['.github/workflows/rust.yml']['events']
        files = subprocess.check_output(['git', 'ls-files'], cwd=ROOT, text=True).splitlines()
        files += ['crates/augmentagent-store/build.rs', 'crates/augmentagent-store/Cargo.toml', 'new-top-level-file']
        disagree = [f for f in files if (targeted.owner(f, crates) is None) != ci.applicable(events, [f])]
        self.assertEqual(disagree, [])
        # Every directory under crates/ is a workspace member, so `!crates/**`
        # never hides a change the planner would call full.
        self.assertEqual(sorted(p.name for p in (ROOT / 'crates').iterdir()),
                         sorted(Path(row['dir']).name for row in crates.values()))

    def test_cross_crate_file_references_are_covered_by_dependents(self):
        # A crate that embeds, includes or reads another crate's files must be
        # one of its dependents, or a change there would not re-run the reader.
        # Deliberately broader patterns than the planner's FILE_REFERENCE.
        crates = targeted.workspace(ROOT)
        by_dir = {row['dir']: name for name, row in crates.items()}
        relative = re.compile(r'"((?:\.\./)+[^"]*)"')
        named = re.compile(r'augmentagent-[a-z0-9-]+')
        missing, scanners = set(), set()
        for reader, row in crates.items():
            for source in (ROOT / row['dir']).rglob('*.rs'):
                text = source.read_text(errors='replace')
                if re.search(r'"crates"', text):
                    scanners.add(reader)
                targets = set()
                for literal in relative.findall(text):
                    target = (source.parent / literal).resolve()
                    if target.is_relative_to(ROOT / 'crates'):
                        targets.add('/'.join(target.relative_to(ROOT).parts[:2]))
                for literal in re.findall(r'"crates/(augmentagent-[a-z0-9-]+)', text):
                    targets.add('crates/' + literal)
                for target_dir in targets:
                    owner = by_dir.get(target_dir)
                    if owner and owner != reader and reader not in targeted.dependents(crates, {owner}):
                        missing.add(reader + ' reads ' + owner)
        self.assertEqual(sorted(missing), [])
        # Any crate whose sources name the crates/ directory itself walks the
        # workspace and must be a declared scanner (re-run on every change).
        self.assertEqual(sorted(scanners - targeted.WORKSPACE_SCANNERS), [])
        for scanner in targeted.WORKSPACE_SCANNERS:
            self.assertIn(scanner, targeted.dependents(crates, {'augmentagent-store'}))

class Wait(unittest.TestCase):
    def expected(self, changed):
        return targeted.expected_jobs(CONTRACT, changed, ci)

    def test_expected_jobs_follow_path_filters_and_skip_macos_and_self(self):
        crate_only = self.expected(['crates/augmentagent-store/src/lib.rs'])
        self.assertNotIn('.github/workflows/rust.yml', crate_only)
        self.assertNotIn('.github/workflows/platform.yml', crate_only)
        self.assertNotIn(targeted.SELF_WORKFLOW, crate_only)
        self.assertIn('privacy', crate_only['.github/workflows/privacy.yml'])
        full = self.expected(['scripts/check-for-updates.sh'])
        self.assertTrue({'channels (ubuntu-latest)', 'phone', 'agents', 'cli', 'legacy-python', 'pdf'} <= full['.github/workflows/rust.yml'])
        self.assertEqual(full['.github/workflows/platform.yml'], {'platform (ubuntu-latest)'})
        for names in full.values():
            self.assertFalse(any('macos' in n for n in names), names)
        self.assertNotIn('.github/workflows/macos.yml', full)

    def test_full_mode_always_requires_the_full_suites(self):
        plan = {'mode': 'full', 'changed': ['crates/not-a-member/src/lib.rs']}
        required = targeted.required_jobs([CONTRACT], plan, ci)
        self.assertIn('phone', required['.github/workflows/rust.yml'])
        self.assertEqual(required['.github/workflows/platform.yml'], {'platform (ubuntu-latest)'})
        targeted_plan = {'mode': 'targeted', 'changed': ['crates/augmentagent-store/src/lib.rs']}
        self.assertNotIn('.github/workflows/rust.yml', targeted.required_jobs([CONTRACT], targeted_plan, ci))

    def test_base_contract_checks_cannot_be_dropped_by_the_head(self):
        head = json.loads(json.dumps(CONTRACT))
        del head['workflows']['.github/workflows/privacy.yml']
        plan = {'mode': 'targeted', 'changed': ['crates/augmentagent-store/src/lib.rs']}
        self.assertNotIn('.github/workflows/privacy.yml', targeted.required_jobs([head], plan, ci))
        self.assertIn('.github/workflows/privacy.yml', targeted.required_jobs([head, CONTRACT], plan, ci))

    def test_empty_diff_requires_only_unfiltered_workflows(self):
        required = targeted.required_jobs([CONTRACT], {'mode': 'none', 'changed': []}, ci)
        self.assertIn('.github/workflows/privacy.yml', required)
        self.assertNotIn('.github/workflows/rust.yml', required)
        self.assertNotIn('.github/workflows/apple-notes.yml', required)

    def test_cli_runs_binary_tests_single_threaded_like_the_suites(self):
        commands = targeted.test_commands(['augmentagent-cli', 'augmentagent-store'])
        self.assertEqual(commands[0][-2:], ['-p', 'augmentagent-store'])
        self.assertIn(['--bins', '--', '--test-threads=1'], [c[-3:] for c in commands])
        self.assertIn(['--test', '*'], [c[-2:] for c in commands])
        self.assertEqual(targeted.test_commands([]), [])

    def test_verdict_uses_latest_attempt_and_fails_closed(self):
        expected = {'w.yml': {'a', 'b'}}
        run = lambda i, n, attempt, **extra: {'id': i, 'path': 'w.yml', 'event': 'pull_request', 'run_number': n, 'run_attempt': attempt, 'status': 'completed', 'pull_requests': [{'number': 7}], **extra}
        job = lambda name, status='completed', conclusion='success': {'name': name, 'status': status, 'conclusion': conclusion}
        check = lambda runs, jobs, pr=7: targeted.verdict(expected, targeted.latest_runs(expected, runs, pr), jobs)
        self.assertEqual(check([], {})[0], 'pending')
        self.assertEqual(check([run(1, 1, 1)], {})[0], 'pending')  # run not finished yet
        self.assertEqual(check([run(1, 1, 1)], {1: [job('a'), job('b')]}), ('pass', []))
        self.assertEqual(check([run(1, 1, 1)], {1: [job('a')]})[0], 'fail')  # finished without a required job
        self.assertEqual(check([run(1, 1, 1, status='in_progress')], {1: [job('a')]})[0], 'pending')  # job not created yet
        # Required Linux jobs done while the run's macOS jobs still run: green now.
        self.assertEqual(check([run(1, 1, 1, status='in_progress')], {1: [job('a'), job('b'), job('b (macos-latest)', 'queued', None)]}), ('pass', []))
        for conclusion in ['failure', 'cancelled', 'skipped', 'timed_out', None]:
            self.assertEqual(check([run(1, 1, 1)], {1: [job('a'), job('b', conclusion=conclusion)]})[0], 'fail', conclusion)
        # A newer attempt supersedes the earlier failure; push runs and runs for another PR never count.
        runs = [run(1, 1, 1), run(1, 1, 2), {**run(3, 9, 1), 'event': 'push'}, run(4, 10, 1, pull_requests=[{'number': 8}])]
        self.assertEqual(check(runs, {1: [job('a'), job('b')], 3: [job('a', conclusion='failure')], 4: [job('a', conclusion='failure')]}), ('pass', []))
        self.assertEqual(check([run(1, 1, 1, path='w.yml@refs/pull/7/merge')], {1: [job('a'), job('b')]}), ('pass', []))

if __name__ == '__main__':
    unittest.main()
