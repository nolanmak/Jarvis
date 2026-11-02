#!/usr/bin/env python3
"""Behavioral regression for #1807, using the real exporter and managed Cargo.

Run explicitly from a registered repository worktree, for example:
  python3 scripts/tests/autoship_cargo_freshness_replay.py --case output

The existing worktree cache is used through cargo-managed.sh. No production CI
image is touched. Expected failures describe the required fixed behavior; a
stale executable is a failing regression, never a successful reproduction.
This host replay does not establish guest freshness or compiled-input authority.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
OLD_DATE = "2000-01-01T00:00:00Z"
CASES = ("output", "compile-error", "assertion", "same-head")


class Deferred(Exception):
    pass


class HarnessError(Exception):
    pass


def execute(argv, *, env, cwd=ROOT):
    return subprocess.run(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                          capture_output=True, text=True, check=False)


def require(result, description):
    if result.returncode:
        raise HarnessError(f"{description} exited {result.returncode}: {result.stderr[-1200:]}")
    return result.stdout.strip()


class Replay:
    def __init__(self, root, case, environment):
        self.repo = root / "repo"
        self.source = root / "export"
        self.case = case
        self.environment = environment
        self.repo.mkdir()
        (self.repo / "src").mkdir()
        self.source.mkdir()
        self.runs = []
        self.exports = []
        self.namespace_only_observed_source = None
        self.package = "autoship-freshness-" + root.name.rsplit("-", 1)[-1]
        (self.repo / "Cargo.toml").write_text(
            f'[package]\nname = "{self.package}"\nversion = "0.0.0"\nedition = "2021"\n\n[workspace]\n')
        (self.repo / "Cargo.lock").write_text(
            f'version = 4\n\n[[package]]\nname = "{self.package}"\nversion = "0.0.0"\n')
        self.filename = "lib.rs" if case == "assertion" else "main.rs"
        self.git("init", "-q")

    def git(self, *args):
        environment = self.environment | {
            "GIT_AUTHOR_DATE": OLD_DATE, "GIT_COMMITTER_DATE": OLD_DATE,
        }
        return require(execute([
            "git", "-c", "core.hooksPath=/dev/null", "-c", "user.name=Freshness Fixture",
            "-c", "user.email=fixture@example.invalid", "-C", str(self.repo), *args,
        ], env=environment), "fixture Git")

    def commit(self, contents):
        (self.repo / "src" / self.filename).write_text(contents)
        self.git("add", ".")
        self.git("commit", "-qm", "synthetic freshness input")
        return self.git("rev-parse", "HEAD")

    def export(self, head):
        # Only this script's private synthetic export is reconstructed. Never
        # delete a Cargo target, CI image, retained workflow or real checkout.
        for child in self.source.iterdir():
            if child.is_dir() and not child.is_symlink():
                shutil.rmtree(child)
            else:
                child.unlink()
        require(execute([
            sys.executable, "-I", str(ROOT / "scripts/autoship-verifier-checkout.py"),
            str(self.repo), head, str(self.source), str(16 * 1024 * 1024),
        ], env=self.environment), "actual source exporter")
        source = self.source / "src" / self.filename
        self.exports.append({"head": head, "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                             "source_mtime_ns": source.stat().st_mtime_ns})

    def cargo(self, operation, *, source=None, clean=False):
        selected_source = self.source if source is None else source
        command = [
            str(ROOT / "scripts/cargo-managed.sh"), operation, "--offline", "--locked", "--quiet",
            "--manifest-path", str(selected_source / "Cargo.toml"),
        ]
        if clean:
            command += ["-p", self.package]
        result = execute(command, env=self.environment)
        if result.returncode == 75:
            raise Deferred("managed admission deferred; no Cargo result is claimed")
        role = "same-path" if source is None else ("clean-control" if clean else "new-path-control")
        self.runs.append({"operation": operation, "source_role": role,
                          "exit_code": result.returncode,
                          "stdout": result.stdout[-2000:], "stderr": result.stderr[-2000:]})
        return result

    def check(self):
        if self.case == "assertion":
            first_source = '#[test]\nfn regression() { assert_eq!(2 + 2, 4); }\n'
            changed_source = '#[test]\nfn regression() { assert_eq!(2 + 2, 5); }\n'
            operation = "test"
        else:
            first_source = 'fn main() { println!("first"); }\n'
            changed_source = ('fn main() { missing_function(); }\n' if self.case == "compile-error"
                              else 'fn main() { println!("other"); }\n')
            operation = "run"
        first = self.commit(first_source)
        self.export(first)
        initial = self.cargo(operation)
        require(initial, "initial source compilation")
        if operation == "run" and initial.stdout.strip() != "first":
            raise HarnessError("initial executable did not run the fixture source")
        if self.case == "same-head":
            # Simulate an earlier step's source mutation, then reconstruct the
            # original same-HEAD checkout. The next execution must use A again.
            (self.source / "src" / self.filename).write_text(changed_source)
            mutated = self.cargo(operation)
            if require(mutated, "mutated fixture compilation") != "other":
                raise HarnessError("mutation control did not execute the changed source")
            self.export(first)
            expected = "first"
        else:
            self.export(self.commit(changed_source))
            expected = "other"
        final = self.cargo(operation)
        def correct(result):
            if self.case == "compile-error":
                return result.returncode != 0 and "E0425" in result.stderr
            if self.case == "assertion":
                return result.returncode != 0 and "assertion `left == right` failed" in result.stdout
            return result.returncode == 0 and result.stdout.strip() == expected

        # Probe pathname isolation separately: Cargo can also reuse stale
        # artifacts after this move. It is evidence, not a presumed fix.
        control = self.source.parent / "control"
        shutil.copytree(self.source, control, symlinks=True)
        self.namespace_only_observed_source = correct(self.cargo(operation, source=control))
        # Clean only this uniquely named synthetic package under the managed
        # build/prune lease. No cache or other package is deleted. This control
        # establishes what an actual compile of these exact old-mtime files
        # does; it cannot make the earlier same-path regression pass.
        require(self.cargo("clean", source=control, clean=True), "synthetic package clean control")
        if not correct(self.cargo(operation, source=control)):
            raise HarnessError("clean compilation control did not observe the actual source")
        return correct(final)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", choices=(*CASES, "all"), default="all")
    parser.add_argument("--report", type=Path, help="optional synthetic JSON receipt")
    args = parser.parse_args()
    os.umask(0o077)
    environment = os.environ | {"CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0"}
    selected = CASES if args.case == "all" else (args.case,)
    report = {"version": 1, "model_calls": 0, "vm_replay_run": False,
              "production_ci_cache_modified": False, "cases": []}
    exit_code = 0
    for case in selected:
        with tempfile.TemporaryDirectory(prefix="autoship-freshness-") as directory:
            replay = Replay(Path(directory), case, environment)
            try:
                passed = replay.check()
                status = "passed" if passed else "failed"
                if not passed:
                    exit_code = max(exit_code, 1)
            except Deferred as error:
                status = "deferred"
                report["deferred_reason"] = str(error)
                exit_code = 75
            except HarnessError as error:
                status = "harness-error"
                report["harness_error"] = str(error)
                exit_code = 2
            report["cases"].append({"case": case, "status": status,
                                    "exports": replay.exports, "runs": replay.runs,
                                    "namespace_only_observed_source": replay.namespace_only_observed_source})
            print(f"{case}: {status}", flush=True)
            if status in ("deferred", "harness-error"):
                break
    if args.report:
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
