/**
 * Thin wrapper around the local Claude Intercept MITM proxy CLI at
 * ~/claude_intercept/src/cli.js. Lets the agent query captured HTTP/S
 * traffic, extract auth tokens, and check proxy status — purely local,
 * never sent to remote sinks.
 */

import { spawn } from "child_process";
import fs from "fs";
import os from "os";
import path from "path";

const DEFAULT_CLI = path.join(os.homedir(), "claude_intercept", "src", "cli.js");

function cliPath(): string {
  return process.env.CLAUDE_INTERCEPT_CLI ?? DEFAULT_CLI;
}

export function interceptAvailable(): boolean {
  try {
    return fs.existsSync(cliPath());
  } catch {
    return false;
  }
}

interface RunResult {
  ok: boolean;
  stdout: string;
  stderr: string;
  code: number;
}

function runCli(args: string[], timeoutMs = 20000): Promise<RunResult> {
  return new Promise((resolve) => {
    const cli = cliPath();
    if (!fs.existsSync(cli)) {
      resolve({ ok: false, stdout: "", stderr: `intercept CLI not found at ${cli}`, code: 127 });
      return;
    }
    const child = spawn("node", [cli, ...args], { env: process.env });
    let stdout = "";
    let stderr = "";
    let done = false;
    const finish = (code: number) => {
      if (done) return;
      done = true;
      resolve({ ok: code === 0, stdout, stderr, code });
    };
    const timer = setTimeout(() => {
      try {
        child.kill("SIGTERM");
      } catch {}
      finish(124);
    }, timeoutMs);
    child.stdout.on("data", (b) => (stdout += b.toString()));
    child.stderr.on("data", (b) => (stderr += b.toString()));
    child.on("error", (e) => {
      stderr += String(e);
      clearTimeout(timer);
      finish(1);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      finish(code ?? 0);
    });
  });
}

function tryParseJson(s: string): any {
  try {
    return JSON.parse(s);
  } catch {
    return null;
  }
}

export type InterceptAction = "status" | "start" | "stop" | "clear" | "export" | "cert";
export type ExportMode = "api-docs" | "auth" | "summary" | "full";

export interface InterceptParams {
  action: InterceptAction;
  mode?: ExportMode;
  host?: string;
  limit?: number;
}

export async function intercept(p: InterceptParams): Promise<any> {
  if (!interceptAvailable()) {
    return {
      ok: false,
      error: `Claude Intercept not installed (looked at ${cliPath()})`,
      hint: "Install from https://github.com/anthropics/claude_intercept or set CLAUDE_INTERCEPT_CLI",
    };
  }

  switch (p.action) {
    case "status": {
      const r = await runCli(["status"]);
      return { ok: r.ok, status: r.stdout.trim(), stderr: r.stderr.trim() };
    }
    case "start": {
      const r = await runCli(["start", "--no-open"]);
      return { ok: r.ok, output: r.stdout.trim(), stderr: r.stderr.trim() };
    }
    case "stop": {
      const r = await runCli(["stop"]);
      return { ok: r.ok, output: r.stdout.trim(), stderr: r.stderr.trim() };
    }
    case "clear": {
      const r = await runCli(["clear"]);
      return { ok: r.ok, output: r.stdout.trim(), stderr: r.stderr.trim() };
    }
    case "cert": {
      const r = await runCli(["cert"]);
      return { ok: r.ok, cert_path: r.stdout.trim(), stderr: r.stderr.trim() };
    }
    case "export": {
      const args = ["export", "--mode", p.mode ?? "summary"];
      if (p.host) args.push("--host", p.host);
      if (p.limit) args.push("--limit", String(p.limit));
      const r = await runCli(args, 60000);
      const parsed = tryParseJson(r.stdout);
      return { ok: r.ok, data: parsed ?? r.stdout, stderr: r.stderr.trim() };
    }
    default:
      return { ok: false, error: `unknown action: ${(p as any).action}` };
  }
}
