/**
 * NDJSON-over-Unix-socket server. Same wire shape as
 * sidecars/browser/sidecar.py — one JSON request per line in, one JSON
 * response per line out, request_id correlates them.
 */

import net from "net";
import path from "path";
import fs from "fs";
import { spawn } from "child_process";
import { groceryRuntimeDir, grocerySocketPath } from "./socketPath.js";
import { GroceryError, classify } from "./errors.js";
import type { GroceryProvider } from "./provider.js";

type Handler = (params: any) => Promise<any>;

const SCHEDULE_HELPER = "/home/nolan-makatche/AugmentAgent/scripts/grocery-schedule.mjs";
const NODE_BIN = "/usr/bin/node";
const SLUG_RE = /^[a-z0-9-]{1,32}$/;

function runHelper(argv: string[]): Promise<any> {
  return new Promise((resolve, reject) => {
    const child = spawn(NODE_BIN, [SCHEDULE_HELPER, ...argv], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (c) => (stdout += c.toString()));
    child.stderr.on("data", (c) => (stderr += c.toString()));
    child.on("error", (e) => reject(new GroceryError("Internal", `helper spawn error: ${e.message}`)));
    child.on("close", (code) => {
      // The helper always prints JSON to stdout (either {ok:true,...} or {ok:false,error:...}).
      // For --list it prints a JSON array. Try to parse and surface errors uniformly.
      const text = stdout.trim();
      if (!text) {
        return reject(new GroceryError("Internal", `helper exited code=${code} with empty output: ${stderr.trim()}`));
      }
      let parsed: any;
      try {
        parsed = JSON.parse(text.split("\n").pop() || text);
      } catch (e: any) {
        return reject(new GroceryError("Internal", `helper non-JSON output: ${text}`));
      }
      if (Array.isArray(parsed)) return resolve(parsed);
      if (parsed && parsed.ok === false) {
        return reject(new GroceryError("Internal", parsed.error || `helper failed (code=${code})`));
      }
      return resolve(parsed);
    });
  });
}

export class GrocerySocketServer {
  private server: net.Server | null = null;
  private ops: Map<string, Handler> = new Map();

  constructor(private provider: GroceryProvider) {
    this.registerDefaultOps();
  }

  private registerDefaultOps() {
    this.ops.set("ping", async () => ({ pong: true, provider: this.provider.id }));
    this.ops.set("session_check", async () => this.provider.sessionCheck());
    this.ops.set("login", async (p) =>
      this.provider.login({ email: p?.email, password: p?.password }),
    );
    this.ops.set("verify_otp", async (p) => {
      if (!p?.code) throw new GroceryError("OTPRequired", "verify_otp requires { code }");
      return this.provider.verifyOtp(String(p.code), p?.channel);
    });
    this.ops.set("search", async (p) => {
      if (!p?.query) throw new GroceryError("Internal", "search requires { query }");
      return this.provider.search(String(p.query), p?.limit);
    });
    this.ops.set("search_batch", async (p) => {
      if (!Array.isArray(p?.queries)) throw new GroceryError("Internal", "search_batch requires { queries: string[] }");
      return { results: await this.provider.searchBatch(p.queries.map(String), p?.limit_per_query) };
    });
    this.ops.set("products_by_id", async (p) => {
      if (!Array.isArray(p?.prodIds)) throw new GroceryError("Internal", "products_by_id requires { prodIds: number[] }");
      return { products: await this.provider.productsById(p.prodIds) };
    });
    this.ops.set("cart_view", async () => this.provider.cartView());
    this.ops.set("cart_add", async (p) => {
      if (!Array.isArray(p?.items)) throw new GroceryError("Internal", "cart_add requires { items: [{ productId, quantity }] }");
      return this.provider.cartAdd(p.items);
    });
    this.ops.set("cart_remove", async (p) => {
      if (p?.productId == null) throw new GroceryError("Internal", "cart_remove requires { productId }");
      await this.provider.cartRemove(p.productId);
      return { removed: p.productId };
    });

    // Scheduling ops — shell out to scripts/grocery-schedule.mjs which
    // owns the systemd user units. The helper validates OnCalendar before
    // writing anything; we still pre-validate the shapes here so we don't
    // spawn for obviously-bad input.
    this.ops.set("schedule_set", async (p) => {
      const kind = p?.kind;
      const oncalendar = p?.oncalendar;
      const label = p?.label;
      if (kind !== "recurring" && kind !== "oneshot") {
        throw new GroceryError("Internal", "schedule_set requires { kind: 'recurring' | 'oneshot' }");
      }
      if (typeof oncalendar !== "string" || !oncalendar.trim()) {
        throw new GroceryError("Internal", "schedule_set requires { oncalendar: non-empty string }");
      }
      const argv = ["--set", "--kind", String(kind), "--oncalendar", String(oncalendar)];
      if (kind === "oneshot") {
        if (typeof label !== "string" || !SLUG_RE.test(label)) {
          throw new GroceryError("Internal", "schedule_set kind=oneshot requires { label: matching ^[a-z0-9-]{1,32}$ }");
        }
        argv.push("--label", label);
      }
      return runHelper(argv);
    });
    this.ops.set("schedule_list", async () => runHelper(["--list"]));
    this.ops.set("schedule_clear", async (p) => {
      const argv = ["--clear"];
      if (p && p.name != null) {
        if (typeof p.name !== "string" || !/^[a-zA-Z0-9@:_.-]+$/.test(p.name)) {
          throw new GroceryError("Internal", "schedule_clear name must be a valid unit name");
        }
        argv.push("--name", p.name);
      }
      return runHelper(argv);
    });
  }

  async listen(): Promise<string> {
    const dir = groceryRuntimeDir();
    fs.mkdirSync(dir, { recursive: true });
    const sock = grocerySocketPath();
    if (fs.existsSync(sock)) fs.unlinkSync(sock);

    this.server = net.createServer((conn) => this.onConnection(conn));
    await new Promise<void>((resolve, reject) => {
      this.server!.once("error", reject);
      this.server!.listen(sock, () => resolve());
    });
    fs.chmodSync(sock, 0o600);
    console.error(`[grocery] listening on ${sock}`);
    return sock;
  }

  async close(): Promise<void> {
    if (this.server) {
      await new Promise<void>((resolve) => this.server!.close(() => resolve()));
      this.server = null;
    }
    const sock = grocerySocketPath();
    if (fs.existsSync(sock)) fs.unlinkSync(sock);
  }

  private onConnection(conn: net.Socket) {
    let buf = "";
    conn.setEncoding("utf8");
    conn.on("data", (chunk: string | Buffer) => {
      buf += chunk;
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        if (line.trim()) this.dispatch(conn, line);
      }
    });
    conn.on("error", (e) => console.error("[grocery] connection error", e.message));
  }

  private async dispatch(conn: net.Socket, line: string) {
    const t0 = Date.now();
    let req: any = null;
    try {
      req = JSON.parse(line);
    } catch (e: any) {
      conn.write(JSON.stringify({ ok: false, error: { kind: "Internal", message: `invalid JSON: ${e.message}` }, elapsed_ms: Date.now() - t0 }) + "\n");
      return;
    }
    const { request_id, op, params, timeout_ms } = req;
    const handler = this.ops.get(op);
    if (!handler) {
      conn.write(JSON.stringify({ request_id, ok: false, error: { kind: "Internal", message: `unknown op: ${op}` }, elapsed_ms: Date.now() - t0 }) + "\n");
      return;
    }

    const timeoutMs = typeof timeout_ms === "number" ? timeout_ms : 60000;
    let timer: NodeJS.Timeout | null = null;
    const timeoutPromise = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new GroceryError("Timeout", `op ${op} exceeded ${timeoutMs}ms`)), timeoutMs);
    });

    try {
      const result = await Promise.race([handler(params ?? {}), timeoutPromise]);
      if (timer) clearTimeout(timer);
      conn.write(JSON.stringify({ request_id, ok: true, result, elapsed_ms: Date.now() - t0 }) + "\n");
    } catch (e) {
      if (timer) clearTimeout(timer);
      const ge = classify(e);
      conn.write(JSON.stringify({
        request_id,
        ok: false,
        error: { kind: ge.kind, message: ge.message, ...(ge.diagnostic ? { diagnostic: ge.diagnostic } : {}) },
        elapsed_ms: Date.now() - t0,
      }) + "\n");
    }
  }
}
