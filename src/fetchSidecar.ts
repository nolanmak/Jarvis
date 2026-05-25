/**
 * Node client for the fetch sidecar's NDJSON-over-Unix-socket protocol.
 * Mirrors the shape served by sidecars/fetch/src/server.ts.
 */

import net from "net";
import path from "path";
import fs from "fs";
import os from "os";
import { randomUUID } from "crypto";

export type LayerId = "http" | "render" | "firecrawl" | "brightdata";

export interface LayerAttempt {
  layer: LayerId;
  ok: boolean;
  reason?: string;
  elapsed_ms: number;
}

export interface FetchResult {
  url: string;
  final_url?: string;
  status?: number;
  title?: string;
  markdown: string;
  html?: string;
  layer_used: LayerId;
  attempts: LayerAttempt[];
  elapsed_ms: number;
}

export interface FetchSidecarError {
  kind: "InvalidUrl" | "Network" | "Blocked" | "Timeout" | "AllLayersFailed" | "ProviderError" | "Internal";
  message: string;
  diagnostic?: string;
}

function defaultSocketPath(): string {
  if (process.env.FETCH_SOCKET) return process.env.FETCH_SOCKET;
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && fs.existsSync(xdg)) return path.join(xdg, "augmentagent", "fetch.sock");
  return path.join(os.tmpdir(), "augmentagent", "fetch.sock");
}

interface Pending {
  resolve: (v: any) => void;
  reject: (e: any) => void;
  timer: NodeJS.Timeout;
}

class FetchSidecarClient {
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
          p.reject(new Error("fetch sidecar disconnected"));
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

  async call<T = any>(op: string, params: Record<string, any> = {}, timeoutMs = 90000): Promise<T> {
    await this.ensureConnected();
    const request_id = randomUUID();
    const frame = { request_id, op, params, timeout_ms: timeoutMs };
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(request_id);
        reject({ kind: "Timeout", message: `fetch sidecar op '${op}' timed out after ${timeoutMs}ms` });
      }, timeoutMs + 1000);
      this.pending.set(request_id, { resolve, reject, timer });
      this.sock!.write(JSON.stringify(frame) + "\n");
    });
  }
}

let shared: FetchSidecarClient | null = null;
function client(): FetchSidecarClient {
  if (!shared) shared = new FetchSidecarClient();
  return shared;
}

export const webFetch = {
  ping: () => client().call<{ pong: boolean }>("ping"),
  fetch: (params: {
    url: string;
    layers?: LayerId[];
    min_quality_chars?: number;
    timeout_ms?: number;
    render_wait_ms?: number;
  }) => client().call<FetchResult>("fetch", params, (params.timeout_ms ?? 90000) + 5000),
};
