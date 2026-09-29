/** Process discovery and guarded signaling for the dashboard Sessions page. */
import { execFileSync } from "child_process";
import fs from "fs";
import path from "path";

export type SessionRow = {
  pid: number;
  ppid: number;
  etime: string;
  tty: string;
  cwd: string;
  cmd: string;
  started: string;
  executable: string;
  uid: number;
};

export type SessionBackend = {
  list(): SessionRow[];
  inspect(pid: number): SessionRow | null;
  signal(pid: number, signal: NodeJS.Signals): void;
  currentUid(): number | undefined;
};

function isClaude(row: SessionRow): boolean {
  return path.basename(row.executable) === "claude";
}

/** `ps -o lstart=` is five words in the C locale; the final comm is free-form. */
export function parseMacPsRows(output: string, uid: number): SessionRow[] {
  const rows: SessionRow[] = [];
  for (const line of output.split("\n")) {
    const match = line.match(/^\s*(\d+)\s+(\d+)\s+(\S+)\s+(\S+)\s+(\d+)\s+([A-Z][a-z]{2}\s+[A-Z][a-z]{2}\s+\d{1,2}\s+\d\d:\d\d:\d\d\s+\d{4})\s+(.+?)\s*$/);
    if (!match) continue;
    const pid = Number(match[1]);
    const ppid = Number(match[2]);
    const owner = Number(match[5]);
    const executable = match[7];
    if (!Number.isSafeInteger(pid) || pid <= 1 || owner !== uid ||
        path.basename(executable) !== "claude") continue;
    rows.push({ pid, ppid, etime: match[3], tty: match[4], cwd: "",
                cmd: executable, started: match[6].replace(/\s+/g, " "),
                executable, uid: owner });
  }
  return rows.sort((a, b) => b.pid - a.pid);
}

function ps(args: string[]): string {
  return execFileSync("ps", ["-ww", ...args], {
    encoding: "utf8", timeout: 2000, maxBuffer: 2 * 1024 * 1024,
    env: { ...process.env, LC_ALL: "C" },
  });
}

function currentUid(): number { return process.getuid?.() ?? -1; }

const MAC_FIELDS = "pid=,ppid=,etime=,tty=,uid=,lstart=,comm=";

export function createMacBackend(runPs: (args: string[]) => string = ps,
                                 uid: () => number = currentUid): SessionBackend {
  return {
    list() {
      try { return parseMacPsRows(runPs(["-A", "-o", MAC_FIELDS]), uid()); }
      catch { throw new Error("macOS process inspection unavailable (ps failed or timed out)"); }
    },
    inspect(pid) {
      try { return parseMacPsRows(runPs(["-p", String(pid), "-o", MAC_FIELDS]), uid())
        .find((row) => row.pid === pid) ?? null; }
      catch { return null; }
    },
    signal: (pid, signal) => process.kill(pid, signal),
    currentUid: uid,
  };
}

const macBackend = createMacBackend();

function linuxInspect(pid: number): SessionRow | null {
  try {
    const raw = fs.readFileSync(`/proc/${pid}/cmdline`);
    const argv = raw.toString("utf8").split("\0").filter(Boolean);
    const comm = fs.readFileSync(`/proc/${pid}/comm`, "utf8").trim();
    // argv0 can be forged with exec -a. The kernel-owned comm is the identity.
    const executable = comm;
    if (executable !== "claude") return null;
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    const close = stat.lastIndexOf(")");
    if (close < 0) return null;
    const fields = stat.slice(close + 2).trim().split(/\s+/);
    const ppid = Number(fields[1]);
    const started = fields[19]; // stat field 22, relative to field 3 after comm.
    const status = fs.readFileSync(`/proc/${pid}/status`, "utf8");
    const uid = Number(status.match(/^Uid:\s+(\d+)/m)?.[1]);
    if (!Number.isSafeInteger(ppid) || !started || !Number.isSafeInteger(uid) ||
        uid !== currentUid()) return null;
    let cwd = "";
    try { cwd = fs.readlinkSync(`/proc/${pid}/cwd`); } catch { /* unavailable */ }
    let etime = "", tty = "?";
    try {
      const meta = ps(["-p", String(pid), "-o", "etime=,tty="]).trim().match(/^(\S+)\s+(\S+)$/);
      if (meta) { etime = meta[1]; tty = meta[2]; }
    } catch { /* metadata is optional */ }
    return { pid, ppid, etime, tty, cwd, cmd: argv.join(" ") || comm,
             started, executable, uid };
  } catch { return null; }
}

const linuxBackend: SessionBackend = {
  list() {
    try {
      return fs.readdirSync("/proc").filter((entry) => /^\d+$/.test(entry))
        .map((entry) => linuxInspect(Number(entry))).filter((row): row is SessionRow => !!row)
        .sort((a, b) => b.pid - a.pid);
    } catch { return []; }
  },
  inspect: linuxInspect,
  signal: (pid, signal) => process.kill(pid, signal),
  currentUid,
};

type StopResult = { status: number; body: { ok?: boolean; pid?: number; signal?: NodeJS.Signals; error?: string } };

export function createSessionManager(backend: SessionBackend = process.platform === "darwin" ? macBackend : linuxBackend) {
  return {
    list(): SessionRow[] { return backend.list(); },
    stop(pid: number, started: string, force: boolean): StopResult {
      if (!Number.isSafeInteger(pid) || pid <= 1 || !started) {
        return { status: 400, body: { error: "invalid pid or session identity" } };
      }
      let row: SessionRow | null;
      try { row = backend.inspect(pid); }
      catch { row = null; }
      if (!row) return { status: 404, body: { error: "process exited or is inaccessible" } };
      if (row.uid !== backend.currentUid() || !isClaude(row)) {
        return { status: 403, body: { error: "pid is not an owned claude process" } };
      }
      if (row.started !== started) {
        return { status: 409, body: { error: "process identity changed; refresh sessions" } };
      }
      const signal: NodeJS.Signals = force ? "SIGKILL" : "SIGTERM";
      try {
        backend.signal(pid, signal);
        return { status: 200, body: { ok: true, pid, signal } };
      } catch (error) {
        const code = (error as NodeJS.ErrnoException).code;
        if (code === "ESRCH") return { status: 404, body: { error: "process already exited" } };
        if (code === "EPERM") return { status: 403, body: { error: "not permitted to signal process" } };
        return { status: 500, body: { error: "could not signal process" } };
      }
    },
  };
}
