/**
 * AugmentAgent renderer sidecar.
 *
 * Node Unix-domain socket server that fronts a long-running Remotion
 * bundle. Talks to the Rust daemon via NDJSON frames over
 * `${XDG_RUNTIME_DIR}/augmentagent/renderer.sock`.
 *
 * Wire protocol — identical envelope to the browser sidecar
 * (see sidecars/browser/sidecar.py):
 *
 *   Request : {"request_id": "<uuid>", "op": "<name>", "params": {...},
 *              "timeout_ms": 120000}
 *   Success : {"request_id": "...", "ok": true,  "result": {...},
 *              "elapsed_ms": 8123}
 *   Failure : {"request_id": "...", "ok": false, "error": {
 *                "kind": "RenderFailed" | "BadProps" | "Timeout"
 *                      | "BundleFailed" | "Internal",
 *                "message": "..." }, "elapsed_ms": 120000}
 *
 * Ops:
 *   ping   -> {"pong": true, "ts": <epoch_s>}
 *   render -> params {props, out_path, codec?}
 *             result {path, bytes, duration_ms}
 *
 * The Remotion bundle is built once on first `render` and the resulting
 * serveUrl is cached for the life of the process; subsequent renders only
 * pay selectComposition + renderMedia. Concurrent request frames are each
 * dispatched to their own task; renders are serialized via an in-process
 * lock (Chromium frame extraction is heavy — one render at a time keeps
 * the box responsive), while `ping` is never blocked.
 */

import { createServer } from 'node:net';
import { mkdir, stat, unlink } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

import { bundle } from '@remotion/bundler';
import { ensureBrowser, renderMedia, selectComposition } from '@remotion/renderer';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const RUNTIME =
  process.env.XDG_RUNTIME_DIR || `/run/user/${process.getuid?.() ?? 1000}`;
const SOCK_PATH =
  process.env.AUGMENTAGENT_RENDERER_SOCK ||
  path.join(RUNTIME, 'augmentagent', 'renderer.sock');

const ENTRY = path.join(__dirname, 'src', 'index.ts');
const COMPOSITION_ID = process.env.AUGMENTAGENT_RENDERER_COMPOSITION || 'ShortCard';

function log(level, msg) {
  process.stderr.write(
    `${new Date().toISOString()} ${level} renderer ${msg}\n`,
  );
}

// ---------------------------------------------------------------------------
// Typed errors — round-tripped to the Rust client as `error.kind` strings.
// ---------------------------------------------------------------------------

class SidecarError extends Error {
  constructor(message, kind = 'Internal') {
    super(message);
    this.kind = kind;
  }
}
const badProps = (m) => new SidecarError(m, 'BadProps');
const renderFailed = (m) => new SidecarError(m, 'RenderFailed');
const bundleFailed = (m) => new SidecarError(m, 'BundleFailed');

// ---------------------------------------------------------------------------
// Bundle cache — bundle once, reuse the serveUrl across every render.
// ---------------------------------------------------------------------------

let _bundlePromise = null;

function ensureBundle() {
  if (_bundlePromise === null) {
    log('INFO', `bundling ${ENTRY}`);
    _bundlePromise = bundle({
      entryPoint: ENTRY,
      // No webpack overrides — keep the React deterministic and offline.
      onProgress: () => {},
    })
      .then((serveUrl) => {
        log('INFO', `bundle ready: ${serveUrl}`);
        return serveUrl;
      })
      .catch((e) => {
        // Reset so a transient failure can be retried on the next call.
        _bundlePromise = null;
        throw bundleFailed(`bundle failed: ${e?.message ?? e}`);
      });
  }
  return _bundlePromise;
}

// Serialize renders: Chromium frame extraction is CPU/RAM heavy and the box
// also runs a browser sidecar. One render at a time.
let _renderChain = Promise.resolve();
function withRenderLock(fn) {
  const run = _renderChain.then(fn, fn);
  // Swallow the result/err on the chain so one failed render doesn't poison
  // the next; the caller still gets the real outcome via `run`.
  _renderChain = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

function opPing() {
  return { pong: true, ts: Date.now() / 1000 };
}

const ALLOWED_CODECS = new Set(['h264', 'h265', 'vp8', 'vp9']);

async function opRender(params) {
  const props = params?.props;
  const outPath = params?.out_path;
  const codec = params?.codec || 'h264';

  if (props === undefined || props === null || typeof props !== 'object') {
    throw badProps("render: 'props' object required");
  }
  if (typeof outPath !== 'string' || outPath.length === 0) {
    throw badProps("render: 'out_path' string required");
  }
  if (!ALLOWED_CODECS.has(codec)) {
    throw badProps(
      `render: unsupported codec '${codec}' (allowed: ${[...ALLOWED_CODECS].join(', ')})`,
    );
  }

  const serveUrl = await ensureBundle();

  // Idempotent + cheap once present; self-heals if setup.sh's
  // ensureBrowser step was skipped.
  try {
    await ensureBrowser();
  } catch (e) {
    throw renderFailed(
      `Chrome Headless Shell unavailable (run sidecars/renderer/setup.sh): ${e?.message ?? e}`,
    );
  }

  await mkdir(path.dirname(path.resolve(outPath)), { recursive: true });

  return withRenderLock(async () => {
    const started = process.hrtime.bigint();
    let composition;
    try {
      composition = await selectComposition({
        serveUrl,
        id: COMPOSITION_ID,
        inputProps: props,
      });
    } catch (e) {
      throw renderFailed(`selectComposition(${COMPOSITION_ID}): ${e?.message ?? e}`);
    }

    try {
      await renderMedia({
        composition,
        serveUrl,
        codec,
        outputLocation: outPath,
        inputProps: props,
      });
    } catch (e) {
      throw renderFailed(`renderMedia: ${e?.message ?? e}`);
    }

    let bytes = 0;
    try {
      bytes = (await stat(outPath)).size;
    } catch (e) {
      throw renderFailed(`render produced no file at ${outPath}: ${e?.message ?? e}`);
    }
    if (bytes === 0) {
      throw renderFailed(`render produced an empty file at ${outPath}`);
    }

    const durationMs = Number((process.hrtime.bigint() - started) / 1000000n);
    return { path: outPath, bytes, duration_ms: durationMs };
  });
}

const OPS = {
  ping: async () => opPing(),
  render: opRender,
};

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

function nowMs() {
  return Number(process.hrtime.bigint() / 1000000n);
}

function errFrame(requestId, kind, message, elapsedMs) {
  return {
    request_id: requestId,
    ok: false,
    error: { kind, message },
    elapsed_ms: elapsedMs,
  };
}

async function dispatch(req) {
  const requestId = req?.request_id ?? '';
  const op = req?.op ?? '';
  const params = req?.params ?? {};
  const timeoutMs = Number(req?.timeout_ms ?? 120000);

  const handler = OPS[op];
  if (!handler) {
    return errFrame(requestId, 'Internal', `unknown op: ${op}`, 0);
  }

  const started = nowMs();
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(
      () => reject(new SidecarError(`op ${op} timed out after ${timeoutMs}ms`, 'Timeout')),
      timeoutMs,
    );
  });

  try {
    const result = await Promise.race([handler(params), timeout]);
    return {
      request_id: requestId,
      ok: true,
      result,
      elapsed_ms: nowMs() - started,
    };
  } catch (e) {
    const kind = e instanceof SidecarError ? e.kind : 'Internal';
    const message = e?.message ?? String(e);
    if (kind === 'Internal') {
      log('ERROR', `op ${op} crashed: ${e?.stack ?? message}`);
    }
    return errFrame(requestId, kind, message, nowMs() - started);
  } finally {
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------
// Server loop — one task per request frame so a long render doesn't
// head-of-line block a ping on another connection.
// ---------------------------------------------------------------------------

function handleClient(sock) {
  log('INFO', 'client connected');
  let buf = '';
  const pending = new Set();

  const send = (resp) => {
    if (!sock.writable) return;
    sock.write(JSON.stringify(resp) + '\n');
  };

  const process_ = async (line) => {
    let req;
    try {
      req = JSON.parse(line);
    } catch (e) {
      send(errFrame('', 'Internal', `bad json: ${e?.message ?? e}`, 0));
      return;
    }
    send(await dispatch(req));
  };

  sock.setEncoding('utf8');
  sock.on('data', (chunk) => {
    buf += chunk;
    let nl;
    while ((nl = buf.indexOf('\n')) !== -1) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (line.trim().length === 0) continue;
      const task = process_(line);
      pending.add(task);
      task.finally(() => pending.delete(task));
    }
  });

  sock.on('error', (e) => log('WARN', `client socket error: ${e?.message ?? e}`));
  sock.on('close', async () => {
    await Promise.allSettled([...pending]);
    log('INFO', 'client disconnected');
  });
}

async function serve() {
  await mkdir(path.dirname(SOCK_PATH), { recursive: true });
  if (existsSync(SOCK_PATH)) {
    await unlink(SOCK_PATH).catch(() => {});
  }

  const server = createServer(handleClient);

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(SOCK_PATH, () => {
      server.removeListener('error', reject);
      resolve();
    });
  });
  log('INFO', `listening on ${SOCK_PATH} (composition=${COMPOSITION_ID})`);

  // Warm the bundle so the first real render isn't also paying bundle cost.
  ensureBundle().catch((e) =>
    log('WARN', `eager bundle failed (will retry on first render): ${e?.message ?? e}`),
  );

  const shutdown = async (signal) => {
    log('INFO', `shutdown signal received: ${signal}`);
    server.close();
    await unlink(SOCK_PATH).catch(() => {});
    process.exit(0);
  };
  process.on('SIGINT', () => shutdown('SIGINT'));
  process.on('SIGTERM', () => shutdown('SIGTERM'));
}

serve().catch((e) => {
  log('ERROR', `fatal: ${e?.stack ?? e}`);
  process.exit(1);
});
