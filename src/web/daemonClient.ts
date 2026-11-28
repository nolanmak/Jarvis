// The only module that opens the daemon's web socket (spec D2, §5.2, §5.3).
// One newline-terminated JSON request and one newline-terminated JSON response
// per connection. Every failure is thrown as a DaemonError with a §5.2.1 code.

import fs from "node:fs";
import net from "node:net";
import { DaemonError } from "./errors";

export const CONNECT_TIMEOUT_MS = 2_000;
export const RESPONSE_TIMEOUT_MS = 10_000;
export const ACTION_RESOLVE_TIMEOUT_MS = 60_000;
export const MAX_RESPONSE_BYTES = 256 * 1024;
const PROTOCOL_VERSION = 1;

export interface WebThread {
  id: string;
  title: string;
  created_at: number;
  updated_at: number;
  changed_at: number;
  pinned: boolean;
  archived: boolean;
}

export type TurnStatus = "queued" | "running" | "done" | "failed" | "cancelled";
export type ApprovalVerb = "approve" | "skip" | "revise";

export interface ApprovalOutcome {
  kind: string;
  summary: string;
  text?: string;
  link?: string;
  local?: string;
  message?: string;
}

export interface ThreadResult { thread: WebThread }
export interface TurnResult { turn_id: string; status: TurnStatus }
export interface ActionResult { outcome: ApprovalOutcome }

/** Test and tuning hooks; production callers pass nothing. */
export interface RequestOptions {
  connectTimeoutMs?: number;
  responseTimeoutMs?: number;
  /** Expected owner of the socket file; defaults to this process's UID. */
  uid?: number;
  connect?: (socketPath: string) => net.Socket;
}

function socketPathFromEnv(): string {
  const socketPath = process.env.AUGMENTAGENT_WEB_SOCKET;
  if (!socketPath) throw new DaemonError("web_disabled", "AUGMENTAGENT_WEB_SOCKET is not set");
  return socketPath;
}

function assertTrustedSocket(socketPath: string, uid: number): void {
  let stat: fs.Stats;
  try {
    stat = fs.lstatSync(socketPath);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      throw new DaemonError("jarvis_offline", "Jarvis web socket is missing");
    }
    throw new DaemonError("internal", "Jarvis web socket cannot be inspected");
  }
  if (!stat.isSocket() || stat.uid !== uid) {
    throw new DaemonError("internal", "Jarvis web socket path is not a socket owned by this user");
  }
}

function parseResponse(raw: string): Record<string, unknown> {
  let body: unknown;
  try {
    body = JSON.parse(raw);
  } catch {
    throw new DaemonError("internal", "Jarvis sent a malformed response");
  }
  if (!body || typeof body !== "object" || Array.isArray(body)) {
    throw new DaemonError("internal", "Jarvis sent a malformed response");
  }
  const res = body as Record<string, unknown>;
  if (res.ok === true) return res;
  if (res.ok === false && typeof res.code === "string" && res.code) {
    throw new DaemonError(res.code, typeof res.error === "string" ? res.error : undefined);
  }
  throw new DaemonError("internal", "Jarvis sent a malformed response");
}

function request(
  op: string,
  args: object,
  defaultResponseTimeoutMs: number,
  opts: RequestOptions = {},
): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    const socketPath = socketPathFromEnv();
    assertTrustedSocket(socketPath, opts.uid ?? process.getuid!());

    const connectTimeoutMs = opts.connectTimeoutMs ?? CONNECT_TIMEOUT_MS;
    const responseTimeoutMs = opts.responseTimeoutMs ?? defaultResponseTimeoutMs;
    const socket = (opts.connect ?? ((p: string) => net.createConnection(p)))(socketPath);
    const chunks: Buffer[] = [];
    let received = 0;
    let connected = false;
    let settled = false;
    let timer: NodeJS.Timeout;

    const finish = (err: DaemonError | null, value?: Record<string, unknown>) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      if (err) reject(err);
      else resolve(value!);
    };

    timer = setTimeout(
      () => finish(new DaemonError("jarvis_offline", "Jarvis did not accept the connection in time")),
      connectTimeoutMs,
    );

    socket.once("connect", () => {
      connected = true;
      clearTimeout(timer);
      timer = setTimeout(
        () => finish(new DaemonError("jarvis_timeout", "Jarvis did not respond in time")),
        responseTimeoutMs,
      );
      socket.write(JSON.stringify({ ...args, v: PROTOCOL_VERSION, op }) + "\n");
    });

    socket.on("data", (chunk: Buffer) => {
      const nl = chunk.indexOf(0x0a);
      const part = nl < 0 ? chunk : chunk.subarray(0, nl);
      received += part.length;
      if (received > MAX_RESPONSE_BYTES) {
        finish(new DaemonError("internal", "Jarvis response exceeds 256 KiB"));
        return;
      }
      chunks.push(part);
      if (nl < 0) return;
      try {
        finish(null, parseResponse(Buffer.concat(chunks).toString("utf8")));
      } catch (err) {
        finish(err as DaemonError);
      }
    });

    socket.on("end", () => finish(new DaemonError("internal", "Jarvis closed the connection without a response")));

    socket.on("error", (err: NodeJS.ErrnoException) => {
      if (!connected) {
        finish(new DaemonError("jarvis_offline", `Jarvis is offline (${err.code ?? err.message})`));
      } else {
        finish(new DaemonError("internal", `Jarvis connection failed (${err.code ?? err.message})`));
      }
    });
  });
}

export async function threadCreate(args: { title?: string }, opts?: RequestOptions): Promise<ThreadResult> {
  const res = await request("thread.create", args, RESPONSE_TIMEOUT_MS, opts);
  return { thread: res.thread as WebThread };
}

export async function threadRename(
  args: { thread_id: string; title: string },
  opts?: RequestOptions,
): Promise<ThreadResult> {
  const res = await request("thread.rename", args, RESPONSE_TIMEOUT_MS, opts);
  return { thread: res.thread as WebThread };
}

export async function threadSet(
  args: { thread_id: string; pinned?: boolean; archived?: boolean },
  opts?: RequestOptions,
): Promise<ThreadResult> {
  const res = await request("thread.set", args, RESPONSE_TIMEOUT_MS, opts);
  return { thread: res.thread as WebThread };
}

export async function threadResetSession(args: { thread_id: string }, opts?: RequestOptions): Promise<ThreadResult> {
  const res = await request("thread.reset_session", args, RESPONSE_TIMEOUT_MS, opts);
  return { thread: res.thread as WebThread };
}

export async function turnSubmit(
  args: { thread_id: string; turn_id: string; text: string },
  opts?: RequestOptions,
): Promise<TurnResult> {
  const res = await request("turn.submit", args, RESPONSE_TIMEOUT_MS, opts);
  return { turn_id: res.turn_id as string, status: res.status as TurnStatus };
}

export async function turnCancel(args: { turn_id: string }, opts?: RequestOptions): Promise<TurnResult> {
  const res = await request("turn.cancel", args, RESPONSE_TIMEOUT_MS, opts);
  return { turn_id: res.turn_id as string, status: res.status as TurnStatus };
}

export async function actionResolve(
  args: { action_id: string; verb: ApprovalVerb; note?: string },
  opts?: RequestOptions,
): Promise<ActionResult> {
  const res = await request("action.resolve", args, ACTION_RESOLVE_TIMEOUT_MS, opts);
  return { outcome: res.outcome as ApprovalOutcome };
}
