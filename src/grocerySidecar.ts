/**
 * Node client for the grocery sidecar's NDJSON-over-Unix-socket protocol.
 * Mirrors the wire shape served by sidecars/grocery/src/server.ts.
 *
 * One connection per client, request_id correlates requests with responses,
 * timeout is enforced both client- and server-side.
 */

import net from "net";
import path from "path";
import fs from "fs";
import os from "os";
import { randomUUID } from "crypto";

export interface GrocerySidecarError {
  kind:
    | "AuthRequired"
    | "OTPRequired"
    | "OutOfStock"
    | "NotFound"
    | "RateLimited"
    | "Timeout"
    | "CartError"
    | "Internal";
  message: string;
  diagnostic?: string;
}

function defaultSocketPath(): string {
  if (process.env.GROCERY_SOCKET) return process.env.GROCERY_SOCKET;
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && fs.existsSync(xdg)) return path.join(xdg, "augmentagent", "grocery.sock");
  return path.join(os.tmpdir(), "augmentagent", "grocery.sock");
}

interface Pending {
  resolve: (v: any) => void;
  reject: (e: any) => void;
  timer: NodeJS.Timeout;
}

class GrocerySidecarClient {
  private sock: net.Socket | null = null;
  private buf = "";
  private connecting: Promise<void> | null = null;
  private pending = new Map<string, Pending>();

  constructor(private socketPath: string = defaultSocketPath()) {}

  private async ensureConnected(): Promise<void> {
    if (this.sock && !this.sock.destroyed) return;
    if (this.connecting) return this.connecting;
    this.connecting = new Promise<void>((resolve, reject) => {
      const s = net.createConnection(this.socketPath);
      s.setEncoding("utf8");
      s.once("connect", () => {
        this.sock = s;
        resolve();
      });
      s.once("error", (e) => {
        this.sock = null;
        reject(e);
      });
      s.on("data", (chunk: string | Buffer) => this.onData(chunk));
      s.on("close", () => {
        this.sock = null;
        for (const [, p] of this.pending) {
          clearTimeout(p.timer);
          p.reject(new Error("grocery sidecar disconnected"));
        }
        this.pending.clear();
      });
    }).finally(() => {
      this.connecting = null;
    });
    return this.connecting;
  }

  private onData(chunk: string | Buffer) {
    this.buf += chunk.toString();
    let nl: number;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (!line.trim()) continue;
      let frame: any;
      try {
        frame = JSON.parse(line);
      } catch {
        continue;
      }
      const id = frame.request_id;
      if (!id) continue;
      const p = this.pending.get(id);
      if (!p) continue;
      this.pending.delete(id);
      clearTimeout(p.timer);
      if (frame.ok) p.resolve(frame.result);
      else p.reject(frame.error ?? { kind: "Internal", message: "unknown error" });
    }
  }

  async call<T = any>(op: string, params: Record<string, any> = {}, timeoutMs = 60000): Promise<T> {
    await this.ensureConnected();
    const request_id = randomUUID();
    const frame = { request_id, op, params, timeout_ms: timeoutMs };
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(request_id);
        reject({ kind: "Timeout", message: `grocery sidecar op '${op}' timed out after ${timeoutMs}ms` });
      }, timeoutMs + 1000);
      this.pending.set(request_id, { resolve, reject, timer });
      this.sock!.write(JSON.stringify(frame) + "\n");
    });
  }

  close() {
    if (this.sock) {
      this.sock.end();
      this.sock = null;
    }
  }
}

let shared: GrocerySidecarClient | null = null;
function client(): GrocerySidecarClient {
  if (!shared) shared = new GrocerySidecarClient();
  return shared;
}

// ── Typed wrappers ────────────────────────────────────────────────────

export interface SessionInfo {
  authenticated: boolean;
  userId?: string | number;
  storeId?: string | number;
  firstName?: string;
}

export interface Product {
  prodId: string | number;
  name: string;
  brand: string;
  price: number;
  regularPrice: number;
  size: string;
  inStock: boolean;
  onSale: boolean;
}

export interface CartItem {
  prodId: string | number;
  name: string;
  brand: string;
  price: number;
  regularPrice: number;
  qty: number;
  size: string;
}

export interface CartDetail {
  cartId: string | number;
  items: CartItem[];
  subtotal: number;
  tax: number;
  total: number;
  storeId: string | number;
}

export type ScheduleKind = "recurring" | "oneshot";

export interface ScheduleSetResult {
  ok: true;
  unit: string;
  next: string | null;
  kind: ScheduleKind;
  oncalendar: string;
}

export interface ScheduleListEntry {
  name: string;
  next: string | null;
  last: string | null;
  kind: ScheduleKind;
}

export interface ScheduleClearResult {
  ok: true;
  cleared: string[];
}

export const grocery = {
  ping: () => client().call<{ pong: boolean; provider: string }>("ping"),
  sessionCheck: () => client().call<SessionInfo>("session_check"),
  login: (email?: string, password?: string) => client().call("login", { email, password }, 120000),
  verifyOtp: (code: string, channel?: string) => client().call("verify_otp", { code, channel }),
  search: (query: string, limit?: number) =>
    client().call<{ query: string; products: Product[]; diagnostic?: string }>("search", { query, limit }),
  searchBatch: (queries: string[], limit_per_query?: number) =>
    client().call<{ results: Array<{ query: string; products: Product[]; diagnostic?: string; error?: string }> }>(
      "search_batch",
      { queries, limit_per_query },
    ),
  productsById: (prodIds: Array<string | number>) =>
    client().call<{ products: Product[] }>("products_by_id", { prodIds }),
  cartView: () => client().call<CartDetail>("cart_view"),
  cartAdd: (items: Array<{ productId: string | number; quantity: number }>) =>
    client().call<{ added: any[]; errors: any[] }>("cart_add", { items }),
  cartRemove: (productId: string | number) => client().call<{ removed: any }>("cart_remove", { productId }),
  scheduleSet: (kind: ScheduleKind, oncalendar: string, label?: string) =>
    client().call<ScheduleSetResult>("schedule_set", { kind, oncalendar, label }),
  scheduleList: () => client().call<ScheduleListEntry[]>("schedule_list"),
  scheduleClear: (name?: string) =>
    client().call<ScheduleClearResult>("schedule_clear", name ? { name } : {}),
};
