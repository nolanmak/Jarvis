import { spawn } from "child_process";
import path from "path";

const MAX_ANSWER_BYTES = 1024 * 1024;
const QUERY_TIMEOUT_MS = 10 * 60 * 1000;

/** Run dashboard questions through the same Rust wiki/Discord tool harness. */
export function runAgentQuery(question: string, signal?: AbortSignal): Promise<string> {
  const repo = path.resolve(__dirname, "..");
  const wiki = path.resolve(process.env.AUGMENTAGENT_WIKI_DIR || path.join(repo, "wiki"));
  const binary = process.env.AUGMENTAGENT_BIN || path.join(repo, "target/release/augmentagent");
  if (!path.isAbsolute(binary)) {
    return Promise.reject(new Error("Jarvis CLI path must be absolute"));
  }
  if (signal?.aborted) {
    return Promise.reject(new Error("Jarvis query was cancelled"));
  }
  return new Promise((resolve, reject) => {
    const child = spawn(binary, ["--wiki-dir", wiki, "wiki", "ask", "--stdin"], {
      cwd: repo,
      env: process.env,
      stdio: ["pipe", "pipe", "ignore"],
      detached: process.platform !== "win32",
    });
    const chunks: Buffer[] = [];
    let bytes = 0;
    let stopped: "timeout" | "cancelled" | "oversized" | null = null;
    let settled = false;
    let killTimer: NodeJS.Timeout | undefined;

    const stop = (reason: "timeout" | "cancelled" | "oversized") => {
      if (stopped) return;
      stopped = reason;
      if (child.pid && process.platform !== "win32") {
        try { process.kill(-child.pid, "SIGTERM"); } catch { /* already exited */ }
        killTimer = setTimeout(() => {
          try { process.kill(-child.pid!, "SIGKILL"); } catch { /* already exited */ }
        }, 5_000);
        killTimer.unref();
      } else {
        child.kill("SIGTERM");
      }
    };
    const abort = () => stop("cancelled");
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    child.stdin.on("error", () => { /* close reports the CLI failure */ });
    child.stdin.end(question);
    const timeout = setTimeout(() => stop("timeout"), QUERY_TIMEOUT_MS);
    timeout.unref();
    const finish = (error?: Error, answer?: string) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      if (killTimer) clearTimeout(killTimer);
      signal?.removeEventListener("abort", abort);
      if (error) reject(error);
      else resolve(answer || "");
    };
    child.stdout.on("data", (chunk: Buffer) => {
      bytes += chunk.length;
      if (bytes > MAX_ANSWER_BYTES) stop("oversized");
      else chunks.push(chunk);
    });
    child.on("error", () => finish(new Error("Jarvis CLI could not start")));
    child.on("close", (code) => {
      if (stopped) return finish(new Error(
        stopped === "timeout" ? "Jarvis query timed out" :
        stopped === "cancelled" ? "Jarvis query was cancelled" : "Jarvis answer exceeded the size limit"));
      if (code !== 0) return finish(new Error(`Jarvis query failed (exit ${code ?? "unknown"})`));
      finish(undefined, Buffer.concat(chunks).toString("utf8").trim());
    });
  });
}
