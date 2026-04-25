// Shell out to the Rust augmentagent CLI for Discord API calls. Keeps the
// token in Keychain (Rust side) — Node never touches it.

import { spawn } from "child_process";
import path from "path";

const CLI_PATH_RELEASE = path.resolve(process.cwd(), "target/release/augmentagent");
const CLI_PATH_DEBUG = path.resolve(process.cwd(), "target/debug/augmentagent");

/**
 * Resolve the augmentagent binary. Prefers release, falls back to debug for
 * local dev. Surface a clear error if neither exists.
 */
function cliPath(): string {
  const fs = require("fs");
  if (fs.existsSync(CLI_PATH_RELEASE)) return CLI_PATH_RELEASE;
  if (fs.existsSync(CLI_PATH_DEBUG)) return CLI_PATH_DEBUG;
  throw new Error(
    "augmentagent binary not found; run `cargo build --release` or `cargo build`"
  );
}

interface RunOptions {
  timeoutMs?: number;
}

/**
 * Run `augmentagent <args...> --json` and parse stdout as JSON.
 * Throws with stderr on non-zero exit.
 */
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
      reject(new Error(`discord CLI timed out after ${opts.timeoutMs ?? 30_000}ms: ${args.join(" ")}`));
    }, opts.timeoutMs ?? 30_000);
    child.on("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error(`discord CLI exited ${code}: ${stderr.trim() || stdout.trim()}`));
        return;
      }
      try {
        resolve(JSON.parse(stdout) as T);
      } catch (e) {
        reject(
          new Error(
            `discord CLI produced non-JSON stdout: ${(e as Error).message}\n---stdout---\n${stdout}`
          )
        );
      }
    });
  });
}

export interface DiscordDmSummary {
  id: string;
  type: number;
  display_name: string;
  is_one_to_one: boolean;
}

export interface DiscordGuildSummary {
  id: string;
  name: string;
}

export interface DiscordGuildChannelSummary {
  id: string;
  name: string;
}

export interface DiscordStatus {
  connected: boolean;
  user_id?: string;
}

export function discordStatus(): Promise<DiscordStatus> {
  return runCliJson<DiscordStatus>(["discord", "status", "--json", "true"]);
}

export function listDms(): Promise<DiscordDmSummary[]> {
  return runCliJson<DiscordDmSummary[]>(["discord", "list-dms", "--json", "true"]);
}

export function listGuilds(): Promise<DiscordGuildSummary[]> {
  return runCliJson<DiscordGuildSummary[]>(["discord", "list-guilds", "--json", "true"]);
}

export function listGuildChannels(guildId: string): Promise<DiscordGuildChannelSummary[]> {
  return runCliJson<DiscordGuildChannelSummary[]>([
    "discord",
    "list-guild-channels",
    guildId,
    "--json",
    "true",
  ]);
}
