#!/usr/bin/env python3
"""Public CLI storage lifecycle regression; all data and state are temporary.
Usage: python3 scripts/tests/disk-storage-smoke.py /absolute/path/to/augmentagent
"""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time

CLI = str(Path(sys.argv[1]).resolve())
TAG = "Signature: 8a477f597d28d172789f06886806bc55\n"

with tempfile.TemporaryDirectory(prefix="jarvis-disk-smoke-") as temporary:
    base = Path(temporary)
    repo = base / "repo"
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    env = dict(os.environ, XDG_STATE_HOME=str(base / "state"),
               AUGMENTAGENT_GATE_TARGET_DIR=str(base / "gate"),
               AUGMENTAGENT_GATE_MIN_FREE_GB="0", AUGMENTAGENT_BUILD_GROWTH_GB="0",
               AUGMENTAGENT_DEPLOY_SNAPSHOT_DIR=str(base / "snapshots"),
               AUGMENTAGENT_PRUNE_TARGET_DIRS="")

    env.pop("AUGMENTAGENT_BUILD_PARENT_LEASE", None)  # Fixture state is independent of an enclosing gate.

    def command(*args):
        result = subprocess.run([CLI, *map(str, args)], cwd=repo, env=env,
                                capture_output=True, text=True)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result.stdout

    def cache(path):
        (path / "debug" / "deps").mkdir(parents=True)
        (path / "CACHEDIR.TAG").write_text(TAG)
        (path / "debug" / "deps" / "artifact").write_bytes(b"cache" * 4096)

    def age_tree(path):
        old = time.time() - 5 * 86400
        for child in path.rglob("*"):
            os.utime(child, (old, old), follow_symlinks=False)
        os.utime(path, (old, old))

    parent = base / "managed"
    command("disk", "manage-parent", "--root", parent)
    owned = parent / "future-session-name"
    cache(owned)
    outside = base / "other-project"
    cache(outside)
    source = parent / "source"
    cache(source)
    (source / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='0.0.0'\n")
    (parent / "symlink-escape").symlink_to(outside, target_is_directory=True)
    age_tree(owned)

    explicit = base / "external-session"
    cache(explicit)
    worktree = base / "removed-worktree"
    worktree.mkdir()
    command("disk", "register", "--root", explicit, "--worktree", worktree)
    worktree.rmdir()
    age_tree(explicit)
    abandoned = base / "abandoned-without-worktree"
    cache(abandoned)
    command("disk", "register", "--root", abandoned, "--abandoned")
    age_tree(abandoned)
    dry = json.loads(command("disk", "prune", "--dry-run", "--json"))
    assert any(r.get("path") == str(owned) and r["full_drop"] for r in dry), dry
    assert any(r.get("path") == str(explicit) and r["full_drop"] for r in dry), dry
    assert owned.exists() and explicit.exists() and abandoned.exists()
    command("disk", "prune", "--json")
    assert not owned.exists() and not explicit.exists() and not abandoned.exists()
    assert outside.exists() and source.exists() and (parent / "symlink-escape").is_symlink()
    print("ok: discovery, removed-worktree retention, dry-run, and ownership boundaries")

    # Moved legacy gate profiles can lack CACHEDIR.TAG. Configuration owns
    # the shared gate already; using it from another repository must neither
    # require a tag nor change an existing ownership record.
    gate = base / "gate"
    (gate / "debug" / "deps").mkdir(parents=True)
    (gate / "debug" / ".fingerprint").mkdir()
    (gate / ".rustc_info.json").write_text(json.dumps({"rustc_fingerprint": 123, "outputs": {}}))
    warm_gate_artifact = gate / "debug" / "deps" / "warm-artifact"
    warm_gate_artifact.write_bytes(b"warm gate cache")
    command("disk", "build", "--target-dir", gate, "--", sys.executable, "-c", "pass")
    assert warm_gate_artifact.exists(), "under-budget gate was wiped during admission"
    command("disk", "register", "--root", gate, "--persistent")
    marker = (gate / ".augmentagent-target.json").read_bytes()
    secondary = base / "secondary-repo"
    subprocess.run(["git", "init", "-q", str(secondary)], check=True)
    subprocess.run([CLI, "disk", "build", "--target-dir", str(gate), "--", sys.executable, "-c", "pass"],
                   cwd=secondary, env=env, check=True, capture_output=True)
    assert marker == (gate / ".augmentagent-target.json").read_bytes(), "shared gate ownership was reassigned"
    assert warm_gate_artifact.exists(), "secondary admission wiped the shared warm cache"
    print("ok: tagless legacy gate admission and shared-cache reuse by another repository")

    database = base / "fixture.db"
    with sqlite3.connect(database) as connection:
        connection.execute("create table fixture(value text)")
        connection.execute("insert into fixture values ('synthetic')")

    first_started = base / "first-started"
    release_first = base / "release-first"
    second_started = base / "second-started"
    first_code = "from pathlib import Path; import sys,time,os; " \
                 "assert Path(os.environ['TMPDIR']).parent == Path(os.environ['CARGO_TARGET_DIR']); " \
                 "Path(os.environ['TMPDIR'], 'leftover').write_bytes(b'scratch'); Path(sys.argv[1]).touch(); " \
                 "exec('while not Path(sys.argv[2]).exists(): time.sleep(0.05)')"
    first = subprocess.Popen([CLI, "disk", "build", "--target-dir", str(base / "first-target"),
                              "--", sys.executable, "-c", first_code, str(first_started), str(release_first)],
                             cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    snapshot_waiter = None
    try:
        deadline = time.monotonic() + 15
        while not first_started.exists():
            assert first.poll() is None, first.communicate()
            assert time.monotonic() < deadline, "first managed build did not start"
            time.sleep(0.05)
        # QA invocations may isolate XDG state while sharing a target. The
        # target lease must still exclude their pruning, even while the live
        # writer is a Python/test process rather than cargo itself.
        isolated_env = dict(env, XDG_STATE_HOME=str(base / "isolated-state"))
        isolated = subprocess.run([CLI, "disk", "prune", "--root", str(base / "first-target"), "--cap-mb", "0", "--json"],
                                  cwd=repo, env=isolated_env, capture_output=True, text=True, check=True)
        isolated_report = json.loads(isolated.stdout)
        assert "lease" in isolated_report[0]["outcome"], isolated_report
        assert (base / "first-target" / "tmp" / "leftover").exists(), "isolated state pruned a live writer"
        snapshot_waiter = subprocess.Popen([CLI, "--db", str(database), "deploy", "snapshot", "--label", "waiting-build"],
                                           cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        time.sleep(0.25)
        assert not (base / "snapshots").exists(), "snapshot allocation overlapped a managed build"
        deferred = json.loads(command("disk", "prune", "--json"))
        assert "deferred" in deferred[0]["outcome"], deferred
        second = subprocess.Popen([CLI, "disk", "build", "--target-dir", str(base / "second-target"),
                                   "--", sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()",
                                   str(second_started)], cwd=repo, env=env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            time.sleep(0.25)
            assert not second_started.exists(), "second writer overlapped the first"
            release_first.touch()
            first_out = first.communicate(timeout=15)
            second_out = second.communicate(timeout=15)
            assert first.returncode == 0, first_out
            assert second.returncode == 0, second_out
            assert second_started.exists()
            snapshot_out = snapshot_waiter.communicate(timeout=15)
            assert snapshot_waiter.returncode == 0, snapshot_out
            assert (base / "snapshots").is_dir()
        finally:
            if second.poll() is None:
                second.terminate()
                second.communicate(timeout=10)
    finally:
        release_first.touch()
        if first.poll() is None:
            first.communicate(timeout=15)
        if snapshot_waiter is not None and snapshot_waiter.poll() is None:
            snapshot_waiter.terminate()
            snapshot_waiter.communicate(timeout=10)
    report = json.loads((base / "state" / "augmentagent" / "disk-reconcile.json").read_text())
    assert "after_free_bytes" in report
    command("disk", "build", "--target-dir", base / "first-target", "--", sys.executable, "-c",
            "from pathlib import Path; import os; assert not Path(os.environ['TMPDIR'], 'leftover').exists()")
    print("ok: builds/snapshots serialize; contained scratch is reclaimed on the next admission")

    # A builder already owns admission. Its nested CLI must prove that its
    # ancestor holds both locks, reuse them without cleanup, and reject a
    # forged proof or a different target rather than bypassing protection.
    nested_script = """
import os, pathlib, subprocess, sys
cli, other = sys.argv[1:]
target = os.environ['CARGO_TARGET_DIR']
keep = pathlib.Path(os.environ['TMPDIR'], 'nested-live-data')
keep.touch()
child = [cli, 'disk', 'build', '--target-dir', target, '--', sys.executable, '-c',
         'import os,pathlib; assert pathlib.Path(os.environ["TMPDIR"], "nested-live-data").exists()']
subprocess.run(child, check=True, capture_output=True, timeout=8)
assert keep.exists()
bad = child.copy()
bad[4] = other
assert subprocess.run(bad, capture_output=True, timeout=8).returncode != 0
spoof = dict(os.environ, AUGMENTAGENT_BUILD_PARENT_LEASE='1:2:3')
assert subprocess.run(child, env=spoof, capture_output=True, timeout=8).returncode != 0
"""
    (base / "wrong-nested-target").mkdir()
    command("disk", "build", "--target-dir", base / "nested-target", "--", sys.executable,
            "-c", nested_script, CLI, base / "wrong-nested-target")
    print("ok: nested builder commands reuse proven ancestor leases; forged/different-target proofs fail")

    nested_pid = base / "nested-child-pid"
    nested_exit = base / "nested-child-exit"
    death_script = """
import os, pathlib, subprocess, sys
cli, pidfile, exitfile = sys.argv[1:]
code = 'import os,pathlib,sys,time; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)'
result = subprocess.run([cli, 'disk', 'build', '--target-dir', os.environ['CARGO_TARGET_DIR'],
                         '--', sys.executable, '-c', code, pidfile])
pathlib.Path(exitfile).write_text(str(result.returncode))
"""
    owner = subprocess.Popen([CLI, "disk", "build", "--target-dir", str(base / "parent-death-target"),
                              "--", sys.executable, "-c", death_script, CLI, str(nested_pid), str(nested_exit)],
                             cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        deadline = time.monotonic() + 10
        while not nested_pid.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert nested_pid.exists(), "nested child did not start"
        owner.kill()
        owner.communicate(timeout=8)
        assert nested_exit.exists() and nested_exit.read_text() != "0", "nested build survived loss of its protecting owner"
        status = Path('/proc') / nested_pid.read_text() / 'stat'
        assert not status.exists() or status.read_text().rsplit(')', 1)[1].split()[0] == 'Z'
    finally:
        if owner.poll() is None:
            owner.kill()
            owner.communicate(timeout=8)
    print("ok: losing the parent lease stops the nested build's subprocess group")

    # The session wrapper chooses and registers a stable target without an
    # operator remembering CARGO_TARGET_DIR.
    shim_dir = base / "bin"
    shim_dir.mkdir()
    shim = shim_dir / "cargo"
    target_report = base / "chosen-target"
    shim.write_text('#!/bin/sh\nprintf "%s" "$CARGO_TARGET_DIR" > "$FIXTURE_TARGET_REPORT"\n')
    shim.chmod(0o700)
    wrapper = Path(__file__).resolve().parents[1] / "cargo-managed.sh"
    wrapper_env = dict(env, PATH=str(shim_dir) + os.pathsep + env["PATH"],
                       AUGMENTAGENT_STORAGE_BIN=CLI, AUGMENTAGENT_BUILD_PARENT=str(base / "session-parent"),
                       FIXTURE_TARGET_REPORT=str(target_report))
    wrapper_env.pop("CARGO_TARGET_DIR", None)
    subprocess.run(["bash", str(wrapper), "check"], cwd=repo, env=wrapper_env, check=True, capture_output=True)
    selected = target_report.read_text()
    assert selected.startswith(str(base / "session-parent"))
    subprocess.run(["bash", str(wrapper), "check"], cwd=repo, env=wrapper_env, check=True, capture_output=True)
    assert selected == target_report.read_text()
    command("disk", "build", "--target-dir", selected, "--", "env",
            "PATH=" + wrapper_env["PATH"], "AUGMENTAGENT_STORAGE_BIN=" + CLI,
            "FIXTURE_TARGET_REPORT=" + str(target_report), "bash", wrapper, "check")
    print("ok: session wrapper automatically registers and reuses its target")

    snapshot = Path(command("--db", database, "deploy", "snapshot", "--label", "smoke").strip().splitlines()[-1])
    command("deploy", "pin", snapshot.name, "--reason", "synthetic rollback")
    assert (snapshot / ".pin").exists()
    restored = base / "restored.db"
    command("deploy", "restore-db", snapshot.name, "--to", restored)
    with sqlite3.connect(restored) as connection:
        assert connection.execute("select value from fixture").fetchone()[0] == "synthetic"
    legacy = base / "state" / "augmentagent" / "historical-rollout"
    legacy.mkdir()
    payload = legacy / "rollback.db"
    payload.write_bytes(b"synthetic rollback")
    adopted = Path(command("deploy", "adopt-rollout", "--path", legacy, "--label", "smoke").strip().splitlines()[-1])
    assert (adopted / ".pin").exists() and payload.read_bytes() == b"synthetic rollback"
    before = sorted(str(p) for p in legacy.rglob("*"))
    command("deploy", "prune", "--dry-run", "--json")
    assert before == sorted(str(p) for p in legacy.rglob("*"))
    assert "[pinned]" in command("deploy", "list")
    print("ok: compressed snapshot restore, explicit adoption, pins, and dry-run retention")
