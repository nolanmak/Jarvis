#!/usr/bin/env bash
# Stress benchmark for deploy snapshot retention (#1815).
#
# Seeds a throwaway snapshot root with many manifests across several source
# databases (one of them deleted), legacy manifests without `source_db`,
# pins and forensic evidence, at ages from minutes to weeks. Then it runs the
# real binary's `deploy prune --dry-run --json` and `deploy prune --json`
# under a small budget and checks the retention invariants:
#
#   * the root ends within budget, or prune reports `nothing evictable`;
#   * nothing pinned, forensic, or holding a live keep slot was removed;
#   * budget evictions are oldest first and stop as soon as the total fits;
#   * the dry run planned exactly what the real run did.
#
# It runs from an empty scratch cwd, so the real `target/release` rollback
# binaries are never in reach, and points AUGMENTAGENT_DEPLOY_SNAPSHOT_DIR at
# the scratch root. Nothing outside the scratch dir is touched.
#
# Usage: scripts/bench/snapshot-retention-bench.sh BINARY [COUNT] [BUDGET_MB]
set -euo pipefail

BIN=$(readlink -f "${1:?usage: $0 BINARY [COUNT] [BUDGET_MB]}")
COUNT=${2:-240}
BUDGET_MB=${3:-12}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/snapshot-bench.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/cwd" "$WORK/snaps" "$WORK/dbs"

python3 - "$WORK" "$COUNT" <<'EOF'
import json, os, random, sys, time, datetime
work, count = sys.argv[1], int(sys.argv[2])
random.seed(1815)
dbs = []
for name in ("main.db", "workflow.db", "worktree.db", "gone.db"):
    p = os.path.join(work, "dbs", name)
    open(p, "wb").write(b"x")
    dbs.append(p)
now = time.time()
for i in range(count):
    age_h = random.choice([random.uniform(0, 47), random.uniform(48, 400)])
    created = now - age_h * 3600
    stamp = datetime.datetime.fromtimestamp(created, datetime.timezone.utc)
    name = stamp.strftime("%Y%m%dT%H%M%SZ") + f"-bench-{i:04d}"
    d = os.path.join(work, "snaps", name)
    os.makedirs(d)
    forensic = random.random() < 0.05
    size = random.randint(20_000, 120_000)
    payload = "forensic.db.gz" if forensic else "data.db.gz"
    with open(os.path.join(d, payload), "wb") as f:
        f.write(os.urandom(size))
    m = {"owner": "augmentagent-snapshot-v1", "created_at": stamp.isoformat(),
         "db_bytes": size * 3, "db_gz_bytes": size, "label": f"bench-{i}"}
    if forensic:
        m.update(kind="forensic-page-image", restorable=False)
        open(os.path.join(d, ".pin"), "w").write("evidence")
    else:
        r = random.random()
        if r < 0.25:
            pass  # legacy: no source_db, no kind
        else:
            m.update(kind="rollback", restorable=True, source_db=random.choice(dbs))
        if random.random() < 0.06:
            open(os.path.join(d, ".pin"), "w").write("owner pin")
    open(os.path.join(d, "manifest.json"), "w").write(json.dumps(m))
    os.utime(d, (created, created))
os.remove(os.path.join(work, "dbs", "gone.db"))  # a vanished source group
EOF

export AUGMENTAGENT_DEPLOY_SNAPSHOT_DIR="$WORK/snaps"
export AUGMENTAGENT_DEPLOY_MAX_GB
AUGMENTAGENT_DEPLOY_MAX_GB=$(python3 -c "print($BUDGET_MB/1024)")
cd "$WORK/cwd"
python3 - "$WORK" > "$WORK/before.json" <<'EOF'
import json, os, sys
root = os.path.join(sys.argv[1], "snaps")
out = {}
for n in os.listdir(root):
    d = os.path.join(root, n)
    m = json.load(open(os.path.join(d, "manifest.json")))
    out[n] = {"manifest": m, "pinned": os.path.exists(os.path.join(d, ".pin"))}
json.dump(out, sys.stdout)
EOF

t0=$(date +%s.%N)
"$BIN" deploy prune --dry-run --json > "$WORK/dry.json"
t1=$(date +%s.%N)
"$BIN" deploy prune --json > "$WORK/real.json"
t2=$(date +%s.%N)

python3 - "$WORK" "$BUDGET_MB" "$t0" "$t1" "$t2" <<'EOF'
import json, os, sys, subprocess
work, budget_mb = sys.argv[1], float(sys.argv[2])
t0, t1, t2 = map(float, sys.argv[3:6])
root = os.path.join(work, "snaps")
before = json.load(open(os.path.join(work, "before.json")))
dry = json.load(open(os.path.join(work, "dry.json")))
real = json.load(open(os.path.join(work, "real.json")))
budget = int(budget_mb / 1024 * 1024**3)
fail = []

def du(p):
    return int(subprocess.check_output(["du", "-sB1", p]).split()[0])

# Dry run plans exactly what the real run does.
view = lambda recs: [(r["path"], r["reason"].replace("would remove", "removed", 1)) for r in recs]
if view(dry) != view(real):
    fail.append("dry-run records differ from the real run")

removed = {os.path.basename(r["path"]): r for r in real if r["removed"]}
for n, r in removed.items():
    b = before[n]
    if b["pinned"]:
        fail.append(f"pinned {n} removed")
    if b["manifest"].get("kind") == "forensic-page-image" and "over budget" in r["reason"]:
        fail.append(f"forensic {n} evicted for budget")

# Keep slots: newest 2 per live source group (legacy retires once 2 tagged
# snapshots are newer than all legacy ones; a vanished db retires its group).
cands = {n: b for n, b in before.items() if b["manifest"].get("kind") != "forensic-page-image"}
def key(n): return before[n]["manifest"]["created_at"]
groups = {}
for n, b in cands.items():
    groups.setdefault(b["manifest"].get("source_db"), []).append(n)
legacy = groups.get(None, [])
newest_legacy = max((key(n) for n in legacy), default=None)
tagged_newer = sum(1 for n, b in cands.items() if b["manifest"].get("source_db") and newest_legacy and key(n) > newest_legacy)
slots = set()
for src, names in groups.items():
    retired = (src is None and tagged_newer >= 2) or (src is not None and not os.path.exists(src))
    if not retired:
        slots.update(sorted(names, key=key, reverse=True)[:2])
for n in removed:
    if n in slots:
        fail.append(f"keep slot {n} removed")

budget_evicted = [n for n, r in removed.items() if "over budget" in r["reason"]]
if budget_evicted:
    newest_evicted = max(key(n) for n in budget_evicted)
    survivors = [n for n in cands if os.path.isdir(os.path.join(root, n))
                 and n not in slots and not before[n]["pinned"]]
    older_survivors = [n for n in survivors if key(n) < newest_evicted]
    if older_survivors:
        fail.append(f"not oldest first: {older_survivors[:3]} survived, newer evicted")

after = sum(du(os.path.join(root, n)) for n in os.listdir(root))
blocked = any(r["kind"] == "budget" for r in real)
if after > budget and not blocked:
    fail.append(f"over budget ({after} > {budget}) without a nothing-evictable report")

age_removed = sum(1 for r in removed.values() if "minimum age" in r["reason"])
print(f"snapshots seeded:      {len(before)} ({sum(1 for b in before.values() if b['pinned'])} pinned, "
      f"{sum(1 for b in before.values() if b['manifest'].get('kind') == 'forensic-page-image')} forensic, "
      f"{len(legacy)} legacy)")
print(f"budget:                {budget/1024**2:.1f} MiB; after prune {after/1024**2:.1f} MiB"
      + (" (blocked: nothing evictable)" if blocked else ""))
print(f"removed:               {len(removed)} ({age_removed} by age, {len(budget_evicted)} for budget)")
print(f"dry-run prune:         {t1 - t0:.3f} s")
print(f"real prune:            {t2 - t1:.3f} s")
if fail:
    print("INVARIANTS FAILED:")
    for f in fail:
        print("  -", f)
    sys.exit(1)
print("invariants:            ok (budget, protections, keep slots, oldest-first, dry==real)")
EOF
