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
 * List conversations in a specific workspace. `teamId` scopes the Composio
 * call to that workspace's connected account; omit only when a single
 * workspace is configured.
 */
export function listConversations(
  types: string = "public_channel,private_channel,im,mpim",
  teamId?: string
): Promise<SlackConversationSummary[]> {
  const args = [
    "slack",
    "list-conversations",
    "--types",
    types,
    "--limit",
    "200",
    "--json",
    "true",
  ];
  if (teamId) {
    args.push("--team-id", teamId);
  }
  return runCliJson<SlackConversationSummary[]>(args);
}

export interface SlackPersistArgs {
  entityId: string;
  connectionId: string;
  composioApiKey: string;
}

/**
 * Persist a freshly-OAuthed Slack workspace via the Rust CLI. Runs
 * `augmentagent slack persist-auth ...`, which probes
 * `SLACK_FETCH_TEAM_INFO` to learn the team_id/team_name, writes the Keychain
 * slot, and upserts the `slack_workspaces` row.
 */
export async function persistSlackAuth(
  a: SlackPersistArgs
): Promise<{ ok: boolean; team_id?: string; team_name?: string; user_id?: string; error?: string }> {
  try {
    const result = await runCliJson<{ ok: boolean; team_id: string; team_name: string; user_id: string }>([
      "slack",
      "persist-auth",
      "--entity-id",
      a.entityId,
      "--connection-id",
      a.connectionId,
      "--composio-api-key",
      a.composioApiKey,
    ]);
    return result;
  } catch (e) {
    return { ok: false, error: (e as Error).message };
  }
}

/**
 * Raw CLI escape hatch. Swallows stdout — callers that need the JSON should
 * go through `runCliJson` via a dedicated helper.
 */
export function runCli(args: string[]): Promise<void> {
  return new Promise((resolve, reject) => {
    const child = spawn(cliPath(), args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: process.env,
    });
    let stderr = "";
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.stdout.on("data", () => {});
    child.on("error", reject);
    child.on("exit", (code) => {
      if (code !== 0) {
        reject(new Error(`slack CLI exited ${code}: ${stderr.trim()}`));
        return;
      }
      resolve();
    });
  });
}
