#!/usr/bin/env node
// grocery-schedule.mjs — create / list / clear systemd user timers that
// fire `node /home/nolan-makatche/AugmentAgent/scripts/grocery-weekly.mjs`
// on either a recurring OnCalendar schedule (single slot, replaces any
// existing recurring unit atomically) or as one-off transient units via
// `systemd-run --user --on-calendar=...`.
//
// CLI shape (all output is a single JSON line on stdout):
//   --set --kind recurring --oncalendar "<spec>"
//   --set --kind oneshot --oncalendar "<spec>" --label "<slug>"
//   --list
//   --clear [--name <unit>]
//
// Slug constraint: --label must match ^[a-z0-9-]{1,32}$.
// OnCalendar is validated via `systemd-analyze calendar` before any units
// are written. User-supplied strings are passed as separate argv tokens to
// child_process.spawn — never shell-interpolated.

import { spawn, spawnSync } from "node:child_process";
import { writeFile, mkdir, readdir, unlink } from "node:fs/promises";
import path from "node:path";
import os from "node:os";

const NODE_BIN = "/usr/bin/node";
const WEEKLY_SCRIPT = "/home/nolan-makatche/AugmentAgent/scripts/grocery-weekly.mjs";
const UNIT_DIR = path.join(os.homedir(), ".config", "systemd", "user");
const RECURRING_BASE = "augmentagent-grocery";
const ONESHOT_PREFIX = "augmentagent-grocery-oneshot-";
const SLUG_RE = /^[a-z0-9-]{1,32}$/;

function emitOk(payload) {
  process.stdout.write(JSON.stringify({ ok: true, ...payload }) + "\n");
  process.exit(0);
}

function emitErr(message, extra = {}) {
  process.stdout.write(JSON.stringify({ ok: false, error: message, ...extra }) + "\n");
  process.exit(1);
}

function parseArgs(argv) {
  const args = {
    set: false,
    list: false,
    clear: false,
    kind: null,
    oncalendar: null,
    label: null,
    name: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
      case "--set":
        args.set = true;
        break;
      case "--list":
        args.list = true;
        break;
      case "--clear":
        args.clear = true;
        break;
      case "--kind":
        args.kind = argv[++i] ?? null;
        break;
      case "--oncalendar":
        args.oncalendar = argv[++i] ?? null;
        break;
      case "--label":
        args.label = argv[++i] ?? null;
        break;
      case "--name":
        args.name = argv[++i] ?? null;
        break;
      default:
        emitErr(`unknown argument: ${a}`);
    }
  }
  return args;
}

// run a command, collect stdout/stderr, return {code, stdout, stderr}.
// argv is an array; no shell.
function run(cmd, argv) {
  return new Promise((resolve) => {
    const child = spawn(cmd, argv, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (c) => (stdout += c.toString()));
    child.stderr.on("data", (c) => (stderr += c.toString()));
    child.on("close", (code) => resolve({ code: code ?? -1, stdout, stderr }));
    child.on("error", (e) => resolve({ code: -1, stdout, stderr: e.message }));
  });
}

async function validateOnCalendar(spec) {
  const res = await run("systemd-analyze", ["calendar", spec]);
  if (res.code !== 0) {
    const err = (res.stderr || res.stdout || `exit ${res.code}`).trim();
    emitErr(`invalid OnCalendar: ${err}`);
  }
  // Try to extract "Next elapse: ..." line for user feedback.
  const m = res.stdout.match(/Next elapse:\s*(.+)/);
  return m ? m[1].trim() : null;
}

async function listMatchingUnits() {
  // list-timers --all enumerates both persistent and transient timers.
  // We grep by name prefix because the glob pattern argument is fragile
  // across systemd versions and `list-units --type=timer` is more reliable.
  const res = await run("systemctl", [
    "--user",
    "list-timers",
    "--all",
    "--no-legend",
    "--no-pager",
  ]);
  if (res.code !== 0) return [];
  const out = [];
  for (const line of res.stdout.split("\n")) {
    if (!line.trim()) continue;
    // Columns are space-separated; the unit name is the column that ends
    // with `.timer`. We split on whitespace and find the .timer token.
    const cols = line.trim().split(/\s+/);
    const unit = cols.find((c) => c.endsWith(".timer"));
    if (!unit) continue;
    if (!unit.startsWith(RECURRING_BASE)) continue;
    // Build {name, next, last, kind} by parsing the leading time fields.
    // list-timers columns: NEXT (date time tz) LEFT (duration) LAST (date time tz) PASSED (duration) UNIT ACTIVATES
    // We don't try to slice precisely — instead we use `show` for accuracy.
    const showed = await run("systemctl", [
      "--user",
      "show",
      unit,
      "--property=NextElapseUSecRealtime",
      "--property=LastTriggerUSec",
    ]);
    let next = null;
    let last = null;
    if (showed.code === 0) {
      for (const ln of showed.stdout.split("\n")) {
        const [k, ...rest] = ln.split("=");
        const v = rest.join("=");
        if (k === "NextElapseUSecRealtime") next = v || null;
        else if (k === "LastTriggerUSec") last = v || null;
      }
    }
    const kind = unit.startsWith(ONESHOT_PREFIX) ? "oneshot" : "recurring";
    out.push({ name: unit, next, last, kind });
  }
  return out;
}

async function cmdSet(args) {
  if (!args.kind) emitErr("--set requires --kind <recurring|oneshot>");
  if (!args.oncalendar || typeof args.oncalendar !== "string" || !args.oncalendar.trim()) {
    emitErr("--set requires --oncalendar <spec>");
  }
  if (args.kind !== "recurring" && args.kind !== "oneshot") {
    emitErr(`invalid --kind: ${args.kind} (must be recurring or oneshot)`);
  }

  const nextElapse = await validateOnCalendar(args.oncalendar);

  if (args.kind === "recurring") {
    return setRecurring(args.oncalendar, nextElapse);
  } else {
    if (!args.label) emitErr("--kind oneshot requires --label <slug>");
    if (!SLUG_RE.test(args.label)) {
      emitErr(`invalid --label: ${args.label} (must match ^[a-z0-9-]{1,32}$)`);
    }
    return setOneshot(args.oncalendar, args.label, nextElapse);
  }
}

async function setRecurring(spec, nextElapse) {
  const unit = `${RECURRING_BASE}.timer`;
  const service = `${RECURRING_BASE}.service`;

  // Atomic: stop+disable any existing recurring timer first.
  await run("systemctl", ["--user", "stop", unit]);
  await run("systemctl", ["--user", "disable", unit]);

  await mkdir(UNIT_DIR, { recursive: true });

  const serviceUnit =
    `[Unit]\n` +
    `Description=AugmentAgent grocery weekly order trigger\n` +
    `After=network-online.target\n` +
    `Wants=network-online.target\n` +
    `\n` +
    `[Service]\n` +
    `Type=oneshot\n` +
    `WorkingDirectory=/home/nolan-makatche/AugmentAgent\n` +
    `ExecStart=${NODE_BIN} ${WEEKLY_SCRIPT}\n` +
    `Environment=PATH=/home/nolan-makatche/.local/bin:/home/nolan-makatche/.cargo/bin:/usr/local/bin:/usr/bin:/bin\n` +
    `StandardOutput=append:/home/nolan-makatche/.local/state/augmentagent/grocery.stdout.log\n` +
    `StandardError=append:/home/nolan-makatche/.local/state/augmentagent/grocery.stderr.log\n`;

  const timerUnit =
    `[Unit]\n` +
    `Description=Trigger AugmentAgent grocery order (${spec})\n` +
    `\n` +
    `[Timer]\n` +
    `OnCalendar=${spec}\n` +
    `Persistent=true\n` +
    `Unit=${service}\n` +
    `\n` +
    `[Install]\n` +
    `WantedBy=timers.target\n`;

  await writeFile(path.join(UNIT_DIR, service), serviceUnit, { mode: 0o644 });
  await writeFile(path.join(UNIT_DIR, unit), timerUnit, { mode: 0o644 });

  let r = await run("systemctl", ["--user", "daemon-reload"]);
  if (r.code !== 0) emitErr(`daemon-reload failed: ${r.stderr.trim()}`);

  r = await run("systemctl", ["--user", "enable", "--now", unit]);
  if (r.code !== 0) emitErr(`enable --now ${unit} failed: ${r.stderr.trim()}`);

  emitOk({ unit, next: nextElapse, kind: "recurring", oncalendar: spec });
}

async function setOneshot(spec, label, nextElapse) {
  const unit = `${ONESHOT_PREFIX}${label}`;
  // systemd-run with --on-calendar creates a transient timer. We pass
  // user input ($spec, $unit) as separate argv tokens.
  const r = await run("systemd-run", [
    "--user",
    `--on-calendar=${spec}`,
    `--unit=${unit}`,
    "bash",
    "-c",
    `${NODE_BIN} ${WEEKLY_SCRIPT}`,
  ]);
  if (r.code !== 0) {
    emitErr(`systemd-run failed: ${(r.stderr || r.stdout).trim()}`);
  }
  emitOk({ unit: `${unit}.timer`, next: nextElapse, kind: "oneshot", oncalendar: spec });
}

async function cmdList() {
  const items = await listMatchingUnits();
  process.stdout.write(JSON.stringify(items) + "\n");
  process.exit(0);
}

async function cmdClear(args) {
  let targets;
  if (args.name) {
    const name = args.name;
    // basic sanity check on the unit name
    if (!/^[a-zA-Z0-9@:_.-]+$/.test(name)) emitErr(`invalid --name: ${name}`);
    if (!name.startsWith(RECURRING_BASE)) emitErr(`--name must start with ${RECURRING_BASE}`);
    targets = [name];
  } else {
    const items = await listMatchingUnits();
    targets = items.map((i) => i.name);
  }

  const cleared = [];
  for (const t of targets) {
    // Stop and disable. Disable can fail for transient units; ignore.
    await run("systemctl", ["--user", "stop", t]);
    await run("systemctl", ["--user", "disable", t]);
    // For transient oneshots, also reset-failed in case the unit lingered.
    if (t.startsWith(ONESHOT_PREFIX)) {
      await run("systemctl", ["--user", "reset-failed", t]);
    }
    cleared.push(t);
  }

  // Remove on-disk unit files for the recurring slot. Transient units
  // don't have on-disk files.
  try {
    const entries = await readdir(UNIT_DIR);
    for (const f of entries) {
      if (!f.startsWith(RECURRING_BASE)) continue;
      // If a specific name was given, only remove its on-disk file.
      if (args.name) {
        const baseNoExt = args.name.replace(/\.(timer|service)$/, "");
        const fBaseNoExt = f.replace(/\.(timer|service)$/, "");
        if (baseNoExt !== fBaseNoExt) continue;
      }
      // Don't try to delete files for transient oneshots — they don't exist.
      if (f.startsWith(ONESHOT_PREFIX)) continue;
      try {
        await unlink(path.join(UNIT_DIR, f));
      } catch (e) {
        if (e && e.code !== "ENOENT") throw e;
      }
    }
  } catch (e) {
    if (e && e.code !== "ENOENT") emitErr(`reading ${UNIT_DIR} failed: ${e.message}`);
  }

  const r = await run("systemctl", ["--user", "daemon-reload"]);
  if (r.code !== 0) emitErr(`daemon-reload failed: ${r.stderr.trim()}`);

  emitOk({ cleared });
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const modes = [args.set, args.list, args.clear].filter(Boolean).length;
  if (modes === 0) emitErr("specify one of --set, --list, --clear");
  if (modes > 1) emitErr("--set, --list, --clear are mutually exclusive");

  if (args.set) await cmdSet(args);
  else if (args.list) await cmdList();
  else if (args.clear) await cmdClear(args);
}

main().catch((e) => emitErr(`fatal: ${e?.message || String(e)}`));
