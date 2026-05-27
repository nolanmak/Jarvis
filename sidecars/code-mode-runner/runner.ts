// AugmentAgent Code Mode runner sidecar.
//
// Runs an LLM-generated TypeScript program inside Deno with **all permissions
// denied** (just `deno run runner.ts` — no `--allow-*` flags). The program is
// fed in on stdin as the FIRST NDJSON line:
//
//   {"program": "<source>", "manifest": ["wiki.draftHint", "draft", ...]}
//
// The runner constructs `globalThis.tools` as a nested Proxy. Every leaf is an
// async function that performs an NDJSON RPC over stdio:
//
//   stdout: {"id": <n>, "call": "<name>", "args": [<...>]}
//   stdin : {"id": <n>, "result": <value>}  | {"id": <n>, "error": "<msg>"}
//
// The runner enforces:
//   - 60s wall-clock timeout on `await main()` (program is killed and the
//     final error frame names the cause as a timeout).
//   - 25-call budget. The 26th `tools.*` call rejects with a structured
//     `call_budget_exceeded` error inside the program (the program may catch
//     it, but it cannot raise the cap).
//
// On success the runner emits `{"final": <returnValue or null>}` to stdout
// and exits 0. On uncaught throw or timeout it emits
// `{"error": {"message": "...", "stack": "..."}}` and exits 1.
//
// See README.md for the full protocol and an end-to-end smoke example.

const TIMEOUT_MS = 60_000;
const CALL_BUDGET = 25;

// --- stdio plumbing ---------------------------------------------------------

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function writeLine(obj: unknown): Promise<void> {
  const line = JSON.stringify(obj) + "\n";
  return Deno.stdout.write(encoder.encode(line)).then(() => {});
}

// Pending RPCs keyed by id.
type Pending = {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
};
const pending = new Map<number, Pending>();
let nextId = 1;
let callCount = 0;
// Buffer for responses the reader sees BEFORE rpc() has registered the
// matching pending entry. Pre-existing tests already cover the same-chunk
// race when execution was synchronous (`eval`); switching to async module
// import (#183) widened the window so the reader can reliably get ahead.
// Keeping a small unclaimed-response map closes that hole without changing
// the wire protocol.
type Reply = { result?: unknown; error?: unknown };
const unclaimedReplies = new Map<number, Reply>();
// Set once stdin closes, so a late rpc() call fails fast rather than
// hanging forever waiting for a reply that can never arrive.
let stdinClosed = false;

// Stdin reader: pulls NDJSON frames forever, dispatches to pending RPCs.
// First line is consumed by `readFirstLine` before the reader loop starts.
let stdinBuf = "";
async function* stdinLines(): AsyncGenerator<string> {
  const chunk = new Uint8Array(8192);
  while (true) {
    // Drain any complete lines already buffered before issuing the next read.
    let idx;
    while ((idx = stdinBuf.indexOf("\n")) >= 0) {
      const line = stdinBuf.slice(0, idx);
      stdinBuf = stdinBuf.slice(idx + 1);
      if (line.length > 0) yield line;
    }
    const n = await Deno.stdin.read(chunk);
    if (n === null) {
      // EOF: emit any trailing partial line.
      if (stdinBuf.length > 0) {
        const last = stdinBuf;
        stdinBuf = "";
        yield last;
      }
      return;
    }
    stdinBuf += decoder.decode(chunk.subarray(0, n));
  }
}

async function readFirstLine(): Promise<string> {
  const chunk = new Uint8Array(8192);
  while (true) {
    const idx = stdinBuf.indexOf("\n");
    if (idx >= 0) {
      const line = stdinBuf.slice(0, idx);
      stdinBuf = stdinBuf.slice(idx + 1);
      return line;
    }
    const n = await Deno.stdin.read(chunk);
    if (n === null) {
      // EOF without newline — yield whatever we have.
      const last = stdinBuf;
      stdinBuf = "";
      return last;
    }
    stdinBuf += decoder.decode(chunk.subarray(0, n));
  }
}

function startReaderLoop(): void {
  (async () => {
    try {
      for await (const line of stdinLines()) {
        let msg: { id?: number; result?: unknown; error?: unknown };
        try {
          msg = JSON.parse(line);
        } catch {
          // Ignore malformed lines from the parent — they can't be matched.
          continue;
        }
        if (typeof msg.id !== "number") continue;
        const p = pending.get(msg.id);
        if (p) {
          pending.delete(msg.id);
          if ("error" in msg && msg.error !== undefined) {
            const m = typeof msg.error === "string" ? msg.error : JSON.stringify(msg.error);
            p.reject(new Error(m));
          } else {
            p.resolve(msg.result ?? null);
          }
        } else {
          // Reply arrived before rpc() registered — stash it. rpc() checks
          // this map first and resolves immediately if a match is waiting.
          unclaimedReplies.set(msg.id, {
            ...(("error" in msg) ? { error: msg.error } : {}),
            ...(("result" in msg) ? { result: msg.result } : {}),
          });
        }
      }
      // Stdin EOF: reject every outstanding RPC so the program can fail fast
      // instead of hanging on `await rpc(...)`.
      stdinClosed = true;
      for (const [, p] of pending) p.reject(new Error("stdin closed"));
      pending.clear();
    } catch (e) {
      stdinClosed = true;
      for (const [, p] of pending) p.reject(e as Error);
      pending.clear();
    }
  })();
}

// --- RPC --------------------------------------------------------------------

async function rpc(name: string, args: unknown[]): Promise<unknown> {
  if (callCount >= CALL_BUDGET) {
    // Don't even emit the call — fail synchronously inside the program.
    const err = new Error(
      `call_budget_exceeded: tool call limit of ${CALL_BUDGET} reached`,
    );
    // deno-lint-ignore no-explicit-any
    (err as any).code = "call_budget_exceeded";
    throw err;
  }
  callCount += 1;
  const id = nextId++;
  // Register the pending entry BEFORE writing — otherwise the reader loop can
  // see the response (already buffered from the parent) while we're awaiting
  // the write, fail to match it, and we'd hang forever.
  const promise = new Promise<unknown>((resolve, reject) => {
    // Same-chunk race: if the reply for `id` already landed before we got
    // here (because the parent pre-buffered it alongside the boot frame),
    // the reader stashed it in `unclaimedReplies`. Resolve/reject from the
    // stash and skip the pending registration entirely.
    const stashed = unclaimedReplies.get(id);
    if (stashed) {
      unclaimedReplies.delete(id);
      if ("error" in stashed && stashed.error !== undefined) {
        const m = typeof stashed.error === "string"
          ? stashed.error
          : JSON.stringify(stashed.error);
        reject(new Error(m));
      } else {
        resolve(stashed.result ?? null);
      }
      return;
    }
    // If stdin already closed and nothing was stashed, fail fast instead
    // of hanging forever.
    if (stdinClosed) {
      reject(new Error("stdin closed"));
      return;
    }
    pending.set(id, { resolve, reject });
  });
  await writeLine({ id, call: name, args });
  return await promise;
}

// --- Proxy builder ----------------------------------------------------------

// Convert the flat manifest (`["wiki.draftHint", "draft", "db.recent"]`) into
// a tree of allowed paths. Leaves are marked with `__leaf: true` and carry the
// full dotted name we'll send on the wire.
type Node = { __leaf?: string; [k: string]: Node | string | undefined };

function buildTree(manifest: string[]): Node {
  const root: Node = {};
  for (const full of manifest) {
    const parts = full.split(".");
    let cur: Node = root;
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      const last = i === parts.length - 1;
      if (last) {
        // Allow both "draft" alone and "draft.something" by keeping both __leaf
        // and child branches if they coexist — but in v1 we don't expect that.
        const existing = cur[part];
        if (existing && typeof existing === "object") {
          (existing as Node).__leaf = full;
        } else {
          cur[part] = { __leaf: full } as Node;
        }
      } else {
        const existing = cur[part];
        if (!existing || typeof existing !== "object") {
          const child: Node = {};
          cur[part] = child;
          cur = child;
        } else {
          cur = existing as Node;
        }
      }
    }
  }
  return root;
}

// Wrap a tree node as a Proxy. If the node has `__leaf`, the node itself is
// also callable (so `tools.draft(...)` works when `draft` is a top-level leaf).
function makeProxy(node: Node, path: string[]): unknown {
  const leafName = node.__leaf;
  // The Proxy target must be a function if we want it callable; otherwise an
  // object. We always use a function target so both shapes work.
  const target = function () {} as unknown as Record<string, unknown>;

  return new Proxy(target, {
    get(_t, prop, _r) {
      if (typeof prop === "symbol") {
        // Let internal symbols (e.g. Symbol.toPrimitive) return undefined so
        // weird coercions don't blow up.
        return undefined;
      }
      if (prop === "then") {
        // Critical: never look thenable, or `await tools.x` will hang.
        return undefined;
      }
      const child = node[prop as string];
      if (!child || typeof child !== "object") {
        throw new Error(
          `tool_not_in_manifest: ${[...path, prop as string].join(".")}`,
        );
      }
      return makeProxy(child as Node, [...path, prop as string]);
    },
    apply(_t, _this, args) {
      if (!leafName) {
        throw new Error(
          `not_callable: tools.${path.join(".")} is a namespace, not a tool`,
        );
      }
      return rpc(leafName, args);
    },
  });
}

// --- main -------------------------------------------------------------------

async function run(): Promise<void> {
  const first = await readFirstLine();
  if (!first) {
    await writeLine({
      error: { message: "empty stdin: missing first NDJSON frame", stack: "" },
    });
    Deno.exit(1);
  }

  let header: { program?: unknown; manifest?: unknown };
  try {
    header = JSON.parse(first);
  } catch (e) {
    await writeLine({
      error: {
        message: `invalid first frame JSON: ${(e as Error).message}`,
        stack: (e as Error).stack ?? "",
      },
    });
    Deno.exit(1);
  }

  const program = header.program;
  const manifest = header.manifest;
  if (typeof program !== "string") {
    await writeLine({
      error: { message: "header.program must be a string", stack: "" },
    });
    Deno.exit(1);
  }
  if (
    !Array.isArray(manifest) ||
    !manifest.every((m) => typeof m === "string")
  ) {
    await writeLine({
      error: { message: "header.manifest must be string[]", stack: "" },
    });
    Deno.exit(1);
  }

  // Install the tools Proxy on globalThis.
  const tree = buildTree(manifest as string[]);
  // deno-lint-ignore no-explicit-any
  (globalThis as any).tools = makeProxy(tree, []);

  startReaderLoop();

  // Execute the program by importing it as a `data:` TypeScript module.
  //
  // History (#183): the previous implementation passed `program` straight
  // through `(globalThis as any).eval(program)`. That works for plain JS,
  // but `eval` has no TypeScript parser — so any program that included a
  // type annotation, a type cast (`x as Foo`, `as unknown as Foo`), an
  // interface declaration, or a generic parameter died with
  // `SyntaxError: Unexpected token ':'` (or `'as'`, `'<'`, …). Since the
  // LLM that generates these programs is told it's writing TypeScript, that
  // failure mode was effectively unavoidable in practice.
  //
  // Deno's module loader strips TS types natively. Loading the program via
  // `import("data:application/typescript;base64,...")` therefore:
  //   1. Parses the program with the real TypeScript compiler (no regex
  //      heuristics, no extra deps, no permissions needed — `data:` URL
  //      imports require zero `--allow-*` flags).
  //   2. Runs in module scope, so top-level `await` works and `globalThis`
  //      is shared with the runner (the `tools` Proxy installed above is
  //      visible to the program).
  //
  // We wrap the user program in a tiny shim that re-exports the awaited
  // return value of `main()`. The conventional contract (per README) is
  // for the program to end with `main();` or `await main();`. We strip the
  // trailing invocation so we don't call `main()` twice, then re-emit it
  // as `export const __result__ = await main();` so we can pluck the
  // resolved value back out via the module's exports.
  let result: unknown = null;
  let timedOut = false;
  let timeoutHandle: number | undefined;

  const timeoutPromise = new Promise<never>((_resolve, reject) => {
    timeoutHandle = setTimeout(() => {
      timedOut = true;
      reject(new Error(`timeout: program exceeded ${TIMEOUT_MS}ms wall clock`));
    }, TIMEOUT_MS);
  });

  try {
    const wrapped = wrapProgramForModule(program);
    const dataUrl = "data:application/typescript;base64," + base64Encode(wrapped);
    // `import(dataUrl)` returns a Promise<Module>; race it against the
    // timeout so a hung program (e.g. infinite loop in top-level code) is
    // killed within the 60s budget instead of leaking forever.
    const moduleResult = (async () => {
      const mod = await import(dataUrl);
      // Prefer the wrapper's __result__ export; fall back to default just
      // in case a future shim variant uses that instead. Either is fine.
      return (mod as Record<string, unknown>).__result__ ??
        (mod as Record<string, unknown>).default ?? null;
    })();
    result = await Promise.race([moduleResult, timeoutPromise]);
    if (timeoutHandle !== undefined) clearTimeout(timeoutHandle);
  } catch (e) {
    if (timeoutHandle !== undefined) clearTimeout(timeoutHandle);
    const err = e as Error;
    await writeLine({
      error: {
        message: err?.message ?? String(err),
        stack: err?.stack ?? "",
        ...(timedOut ? { kind: "timeout" } : {}),
      },
    });
    Deno.exit(1);
  }

  await writeLine({ final: result ?? null });
  Deno.exit(0);
}

// Strip a trailing `main();` or `await main();` invocation (with optional
// whitespace and surrounding semicolons / line terminators) from the end of
// the program. We do this so the wrapper can safely append its own
// `export const __result__ = await main();` without calling `main()` twice.
//
// If the program doesn't end with a `main()` call (e.g. it's a one-shot
// top-level await script with no helper function), we leave it alone and
// the wrapper's `typeof main === "function"` guard kicks in.
// Match a trailing `main();` / `await main();` call. The lookbehind for a
// statement-terminator (`;`, `{`, `}`, `\n`, or start-of-string) prevents
// us from chopping into something like `submain();` or `mymain();` that
// happens to share a suffix.
const TRAILING_MAIN_CALL = /(?<=^|[;{}\n])\s*(?:await\s+)?main\s*\(\s*\)\s*;?\s*$/;

export function wrapProgramForModule(program: string): string {
  const stripped = program.replace(TRAILING_MAIN_CALL, "\n");
  // The shim runs in the module's top-level (so `await` is legal) and only
  // calls `main()` if the user defined one. Otherwise we just re-export
  // `undefined` — covers programs that did all their work at top level.
  // `// @ts-ignore` because in a sandboxed program `main` is not typed.
  return `${stripped}
// ---- runner shim (injected) ----
// @ts-ignore -- main is defined (or not) by the user program above
export const __result__ = (typeof main === "function") ? await main() : undefined;
`;
}

// Tiny base64 encoder for UTF-8 strings. `btoa` only accepts Latin-1, so for
// programs that contain non-ASCII chars (emoji, accented names in string
// literals) we encode through TextEncoder first. Avoids importing anything.
function base64Encode(s: string): string {
  const bytes = new TextEncoder().encode(s);
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary);
}

if (import.meta.main) {
  run();
}

// Exported for unit tests.
export const __internal = {
  buildTree,
  makeProxy,
  wrapProgramForModule,
  TIMEOUT_MS,
  CALL_BUDGET,
};
