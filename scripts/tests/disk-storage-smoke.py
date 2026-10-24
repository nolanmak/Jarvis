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

    def command(*args):
        return subprocess.run([CLI, *map(str, args)], cwd=repo, env=env,
                              capture_output=True, text=True, check=True).stdout

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
    dry = json.loads(command("disk", "prune", "--dry-run", "--json"))
    assert any(r.get("path") == str(owned) and r["full_drop"] for r in dry), dry
    assert any(r.get("path") == str(explicit) and r["full_drop"] for r in dry), dry
    assert owned.exists() and explicit.exists()
    command("disk", "prune", "--json")
    assert not owned.exists() and not explicit.exists()
    assert outside.exists() and source.exists() and (parent / "symlink-escape").is_symlink()
    print("ok: discovery, removed-worktree retention, dry-run, and ownership boundaries")

    first_started = base / "first-started"
    release_first = base / "release-first"
    second_started = base / "second-started"
    first_code = "from pathlib import Path; import sys,time; Path(sys.argv[1]).touch(); " \
                 "exec('while not Path(sys.argv[2]).exists(): time.sleep(0.05)')"
    first = subprocess.Popen([CLI, "disk", "build", "--target-dir", str(base / "first-target"),
                              "--", sys.executable, "-c", first_code, str(first_started), str(release_first)],
                             cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        deadline = time.monotonic() + 15
        while not first_started.exists():
            assert first.poll() is None, first.communicate()
            assert time.monotonic() < deadline, "first managed build did not start"
            time.sleep(0.05)
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
        finally:
            if second.poll() is None:
                second.terminate()
                second.communicate(timeout=10)
    finally:
        release_first.touch()
        if first.poll() is None:
            first.communicate(timeout=15)
    report = json.loads((base / "state" / "augmentagent" / "disk-reconcile.json").read_text())
    assert "after_free_bytes" in report
    print("ok: competing build admissions serialize, prune defers, next admission retries cleanup")

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
    print("ok: session wrapper automatically registers and reuses its target")

    database = base / "fixture.db"
    with sqlite3.connect(database) as connection:
        connection.execute("create table fixture(value text)")
        connection.execute("insert into fixture values ('synthetic')")
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
