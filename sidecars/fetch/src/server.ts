import net from "net";
import fs from "fs";
import { fetchRuntimeDir, fetchSocketPath } from "./socketPath.js";
import { FetchError, classify } from "./errors.js";
import type { LayeredFetcher } from "./fetcher.js";

type Handler = (params: any) => Promise<any>;

export class FetchSocketServer {
  private server: net.Server | null = null;
  private ops: Map<string, Handler> = new Map();

  constructor(private fetcher: LayeredFetcher) {
    this.registerOps();
  }

  private registerOps() {
    this.ops.set("ping", async () => ({ pong: true }));
    this.ops.set("fetch", async (p) => {
      if (!p?.url) throw new FetchError("InvalidUrl", "fetch requires { url }");
      return this.fetcher.fetch({
        url: String(p.url),
        layers: Array.isArray(p.layers) ? p.layers : undefined,
        min_quality_chars: typeof p.min_quality_chars === "number" ? p.min_quality_chars : undefined,
        timeout_ms: typeof p.timeout_ms === "number" ? p.timeout_ms : undefined,
        render_wait_ms: typeof p.render_wait_ms === "number" ? p.render_wait_ms : undefined,
      });
    });
  }

  async listen(): Promise<string> {
    fs.mkdirSync(fetchRuntimeDir(), { recursive: true });
    const sock = fetchSocketPath();
    if (fs.existsSync(sock)) fs.unlinkSync(sock);

    this.server = net.createServer((conn) => this.onConnection(conn));
    await new Promise<void>((resolve, reject) => {
      this.server!.once("error", reject);
      this.server!.listen(sock, () => resolve());
    });
    fs.chmodSync(sock, 0o600);
    console.error(`[fetch] listening on ${sock}`);
    return sock;
  }

  async close(): Promise<void> {
    if (this.server) {
      await new Promise<void>((resolve) => this.server!.close(() => resolve()));
      this.server = null;
    }
    const sock = fetchSocketPath();
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
    conn.on("error", (e) => console.error("[fetch] connection error", e.message));
  }

  private async dispatch(conn: net.Socket, line: string) {
    const t0 = Date.now();
    let req: any = null;
    try {
      req = JSON.parse(line);
    } catch (e: any) {
      conn.write(
        JSON.stringify({ ok: false, error: { kind: "Internal", message: `invalid JSON: ${e.message}` }, elapsed_ms: Date.now() - t0 }) + "\n",
      );
      return;
    }
    const { request_id, op, params, timeout_ms } = req;
    const handler = this.ops.get(op);
    if (!handler) {
      conn.write(
        JSON.stringify({ request_id, ok: false, error: { kind: "Internal", message: `unknown op: ${op}` }, elapsed_ms: Date.now() - t0 }) + "\n",
      );
      return;
    }

    const timeoutMs = typeof timeout_ms === "number" ? timeout_ms : 90000;
    let timer: NodeJS.Timeout | null = null;
    const timeoutPromise = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new FetchError("Timeout", `op ${op} exceeded ${timeoutMs}ms`)), timeoutMs);
    });

    try {
      const result = await Promise.race([handler(params ?? {}), timeoutPromise]);
      if (timer) clearTimeout(timer);
      conn.write(JSON.stringify({ request_id, ok: true, result, elapsed_ms: Date.now() - t0 }) + "\n");
    } catch (e) {
      if (timer) clearTimeout(timer);
      const fe = classify(e);
      conn.write(
        JSON.stringify({
          request_id,
          ok: false,
          error: { kind: fe.kind, message: fe.message, ...(fe.diagnostic ? { diagnostic: fe.diagnostic } : {}) },
          elapsed_ms: Date.now() - t0,
        }) + "\n",
      );
    }
  }
}
