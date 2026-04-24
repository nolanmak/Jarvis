// Shell out to the Rust augmentagent CLI for Slack Composio calls. Mirrors
// discordApi.ts so the Slack token never leaves Keychain.

import { spawn } from "child_process";
import path from "path";

const CLI_PATH_RELEASE = path.resolve(process.cwd(), "target/release/augmentagent");
const CLI_PATH_DEBUG = path.resolve(process.cwd(), "target/debug/augmentagent");

function cliPath(): string {
  const fs = require("fs");
  if (fs.existsSync(CLI_PATH_RELEASE)) return CLI_PATH_RELEASE;
  if (fs.existsSync(CLI_PATH_DEBUG)) return CLI_PATH_DEBUG;
  throw new Error("augmentagent binary not found; run `cargo build --release`");
}

interface RunOptions {
  timeoutMs?: number;
}

function runCliJson<T>(args: string[], opts: RunOptions = {}): Promise<T> {
  return new Promise((resolve, reject) => {
    const child = spawn(cliPath(), args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: process.env,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    const timer = setTimeout(() => {
      child.kill("SIGTERM");
      reject(new Error(`slack CLI timed out after ${opts.timeoutMs ?? 30_000}ms: ${args.join(" ")}`));
    }, opts.timeoutMs ?? 30_000);
    child.on("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error(`slack CLI exited ${code}: ${stderr.trim() || stdout.trim()}`));
        return;
      }
      try {
        resolve(JSON.parse(stdout) as T);
      } catch (e) {
        reject(
          new Error(
            `slack CLI produced non-JSON stdout: ${(e as Error).message}\n---stdout---\n${stdout}`
          )
        );
      }
    });
  });
}

export interface SlackConversationSummary {
  id: string;
  name: string;
  display_name: string;
  is_im: boolean;
  is_mpim: boolean;
  is_private: boolean;
}

/**
 * List conversations the authenticated Slack user can see. `types` filters
 * which kinds to include; the dashboard typically splits this call into two
 * (DMs vs channels) for the picker UI.
 */
export function listConversations(
  types: string = "public_channel,private_channel,im,mpim"
): Promise<SlackConversationSummary[]> {
  return runCliJson<SlackConversationSummary[]>([
    "slack",
    "list-conversations",
    "--types",
    types,
    "--limit",
    "200",
    "--json",
    "true",
  ]);
}
