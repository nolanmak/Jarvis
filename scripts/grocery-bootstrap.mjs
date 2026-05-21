#!/usr/bin/env node
// grocery-bootstrap.mjs — one-time interactive setup for the grocery
// sidecar. Spawns it long enough to drive an OTP login, which persists
// cookies into the Chrome profile so subsequent runs are silent.
//
// Run once before the first order:
//
//     npm run grocery:bootstrap
//
// Requires env vars: GIANT_EMAIL, GIANT_PASSWORD, GIANT_STORE_ID.

import { spawn } from "node:child_process";
import { createConnection } from "node:net";
import { createInterface } from "node:readline";
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import os from "node:os";

const REPO = path.resolve(path.dirname(new URL(import.meta.url).pathname), "..");
const SIDECAR_DIR = path.join(REPO, "sidecars", "grocery");
const SIDECAR_ENTRY = path.join(SIDECAR_DIR, "dist", "index.js");

function socketPath() {
  if (process.env.GROCERY_SOCKET) return process.env.GROCERY_SOCKET;
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && existsSync(xdg)) return path.join(xdg, "augmentagent", "grocery.sock");
  return path.join(os.tmpdir(), "augmentagent", "grocery.sock");
}

function prompt(question) {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  return new Promise((resolve) => {
    rl.question(question, (answer) => {
      rl.close();
      resolve(answer.trim());
    });
  });
}

function connectSidecar(sock, attempts = 20) {
  return new Promise((resolve, reject) => {
    let tries = 0;
    const tryOnce = () => {
      const c = createConnection(sock);
      c.once("connect", () => resolve(c));
      c.once("error", () => {
        tries++;
        if (tries >= attempts) return reject(new Error("could not connect to grocery sidecar"));
        setTimeout(tryOnce, 500);
      });
    };
    tryOnce();
  });
}

function call(conn, op, params = {}, timeoutMs = 120000) {
  return new Promise((resolve, reject) => {
    const request_id = randomUUID();
    const frame = { request_id, op, params, timeout_ms: timeoutMs };
    let buf = "";
    const onData = (chunk) => {
      buf += chunk.toString();
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        if (!line.trim()) continue;
        const f = JSON.parse(line);
        if (f.request_id !== request_id) continue;
        conn.off("data", onData);
        if (f.ok) resolve(f.result);
        else reject(f.error);
        return;
      }
    };
    conn.on("data", onData);
    conn.write(JSON.stringify(frame) + "\n");
  });
}

async function main() {
  if (!existsSync(SIDECAR_ENTRY)) {
    console.error(`Sidecar not built. Run:  cd sidecars/grocery && npm install && npm run build`);
    process.exit(1);
  }
  if (!process.env.GIANT_EMAIL || !process.env.GIANT_PASSWORD || !process.env.GIANT_STORE_ID) {
    console.error("Set GIANT_EMAIL, GIANT_PASSWORD, GIANT_STORE_ID in .env first.");
    process.exit(1);
  }

  const sock = socketPath();
  mkdirSync(path.dirname(sock), { recursive: true });

  console.log("[bootstrap] starting grocery sidecar...");
  const child = spawn("node", [SIDECAR_ENTRY], {
    cwd: SIDECAR_DIR,
    env: process.env,
    stdio: ["ignore", "inherit", "inherit"],
  });

  const cleanup = () => {
    try {
      child.kill("SIGTERM");
    } catch {}
  };
  process.on("SIGINT", () => {
    cleanup();
    process.exit(130);
  });

  try {
    const conn = await connectSidecar(sock);
    console.log("[bootstrap] connected");

    const session = await call(conn, "session_check");
    if (session.authenticated) {
      console.log(`[bootstrap] already authenticated as userId=${session.userId}. nothing to do.`);
      cleanup();
      return;
    }

    console.log("[bootstrap] not authenticated — running login...");
    const login = await call(conn, "login", {});

    if (login.status === "success") {
      console.log(`[bootstrap] logged in. userId=${login.userId}`);
      cleanup();
      return;
    }

    if (login.status === "otp_sent") {
      console.log(`[bootstrap] OTP sent via ${login.channel} (${login.maskedValue}).`);
      const code = await prompt("Enter the OTP code: ");
      const verified = await call(conn, "verify_otp", { code });
      console.log(`[bootstrap] OTP verified. userId=${verified.userId}`);
      cleanup();
      return;
    }

    console.error(`[bootstrap] unexpected login result: ${JSON.stringify(login)}`);
    cleanup();
    process.exit(1);
  } catch (e) {
    console.error(`[bootstrap] error:`, e);
    cleanup();
    process.exit(1);
  }
}

main();
