// Acceptance tests for the code-mode Deno runner.
//
// Run with:
//   deno test --allow-run=deno --allow-read=. runner_test.ts
//
// The runner under test is spawned as a subprocess **with no `--allow-*`
// flags** so we exercise the same isolation profile the production sidecar
// uses. The harness itself needs `--allow-run` (to spawn `deno`) and
// `--allow-read` (so Deno can load `runner.ts` from disk).

// deno-lint-ignore-file no-import-prefix
// The runner itself forbids imports (deno.json has empty `imports`). This
// test file is loaded only by `deno test`, never by the production runner,
// so we pull std/assert inline via a JSR specifier.
import { assert, assertEquals, assertStringIncludes } from "jsr:@std/assert@1";

const RUNNER = new URL("./runner.ts", import.meta.url).pathname;

type Frame = Record<string, unknown>;

// Spawn the runner, drive it line-by-line, and return all stdout/stderr.
async function driveRunner(opts: {
  header: { program: string; manifest: string[] };
  // Called with each NDJSON object the runner emits. Return a frame to write
  // back (e.g. an `{id, result}` response to an `{id, call, ...}` request).
  // Returning `undefined` means "don't reply (yet)".
  onFrame: (frame: Frame) => Frame | undefined;
  // Optional hard wall clock for the test itself. The runner has its own 60s
  // timeout; this is just so a hung test doesn't hang the suite forever.
  testTimeoutMs?: number;
}): Promise<{
  stdoutFrames: Frame[];
  stdoutLeftover: string;
  stderr: string;
  status: Deno.CommandStatus;
}> {
  const cmd = new Deno.Command(Deno.execPath(), {
    args: ["run", RUNNER],
    stdin: "piped",
    stdout: "piped",
    stderr: "piped",
    // Deny everything to the child: no --allow-* means default-deny in Deno 2.
  });
  const child = cmd.spawn();

  // Write the header.
  const writer = child.stdin.getWriter();
  await writer.write(
    new TextEncoder().encode(JSON.stringify(opts.header) + "\n"),
  );

  // Read stdout NDJSON, dispatch to onFrame, possibly write responses.
  const stdoutFrames: Frame[] = [];
  const reader = child.stdout.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  const testDeadline = opts.testTimeoutMs
    ? Date.now() + opts.testTimeoutMs
    : Number.POSITIVE_INFINITY;

  let stdinClosed = false;
  while (true) {
    if (Date.now() > testDeadline) {
      try {
        child.kill("SIGKILL");
      } catch { /* already dead */ }
      throw new Error("test wall clock exceeded");
    }
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value);
    let idx;
    while ((idx = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, idx);
      buf = buf.slice(idx + 1);
      if (line.length === 0) continue;
      const frame = JSON.parse(line) as Frame;
      stdoutFrames.push(frame);
      const reply = opts.onFrame(frame);
      if (reply !== undefined && !stdinClosed) {
        await writer.write(
          new TextEncoder().encode(JSON.stringify(reply) + "\n"),
        );
      }
      if ("final" in frame || "error" in frame) {
        // Runner is about to exit — release stdin so it can drain.
        if (!stdinClosed) {
          stdinClosed = true;
          try {
            await writer.close();
          } catch { /* already closed */ }
        }
      }
    }
  }

  if (!stdinClosed) {
    try {
      await writer.close();
    } catch { /* ignore */ }
  }
  reader.releaseLock();

  const stderrBytes = await new Response(child.stderr).bytes();
  const status = await child.status;
  return {
    stdoutFrames,
    stdoutLeftover: buf,
    stderr: new TextDecoder().decode(stderrBytes),
    status,
  };
}

// ---------------------------------------------------------------------------
// Acceptance: smoke — single tools.draft call round-trips, final:null, exit 0.
// ---------------------------------------------------------------------------
Deno.test("smoke: single tools.draft call -> final:null, exit 0", async () => {
  const result = await driveRunner({
    header: {
      program: 'async function main(){ await tools.draft("gmail","hi","r"); } main();',
      manifest: ["draft"],
    },
    onFrame: (f) => {
      if ("call" in f) return { id: f.id, result: null };
      return undefined;
    },
    testTimeoutMs: 10_000,
  });

  assertEquals(result.status.code, 0, `stderr: ${result.stderr}`);
  // Exactly one call frame, exactly one final frame.
  const calls = result.stdoutFrames.filter((f) => "call" in f);
  const finals = result.stdoutFrames.filter((f) => "final" in f);
  assertEquals(calls.length, 1);
  assertEquals(finals.length, 1);
  assertEquals(calls[0].call, "draft");
  assertEquals(calls[0].args, ["gmail", "hi", "r"]);
  assertEquals(finals[0].final, null);
});

// ---------------------------------------------------------------------------
// Acceptance: timeout — a 70s sleep is killed in <61s with a timeout frame.
// ---------------------------------------------------------------------------
Deno.test({
  name: "timeout: 70s sleep is killed in <61s with timeout error",
  sanitizeResources: false,
  fn: async () => {
    const start = Date.now();
    const result = await driveRunner({
      header: {
        program: "async function main(){ await new Promise(r => setTimeout(r, 70000)); } main();",
        manifest: [],
      },
      onFrame: () => undefined,
      testTimeoutMs: 90_000,
    });
    const elapsed = Date.now() - start;

    assert(elapsed < 61_000, `runner took ${elapsed}ms, must be <61_000ms`);
    assertEquals(result.status.code, 1, `stderr: ${result.stderr}`);
    const errFrames = result.stdoutFrames.filter((f) => "error" in f);
    assertEquals(errFrames.length, 1);
    const err = errFrames[0].error as { message: string; kind?: string };
    assertStringIncludes(err.message.toLowerCase(), "timeout");
  },
});

// ---------------------------------------------------------------------------
// Acceptance: budget — 26th tool call is rejected with call_budget_exceeded.
// ---------------------------------------------------------------------------
Deno.test("budget: 26th call rejected with call_budget_exceeded", async () => {
  const program = `
    async function main(){
      let caught = null;
      let succeeded = 0;
      for (let i = 0; i < 26; i++) {
        try {
          await tools.ping();
          succeeded++;
        } catch (e) {
          caught = (e && e.message) || String(e);
          break;
        }
      }
      return { succeeded, caught };
    }
    main();
  `;
  const result = await driveRunner({
    header: { program, manifest: ["ping"] },
    onFrame: (f) => {
      if ("call" in f) return { id: f.id, result: "ok" };
      return undefined;
    },
    testTimeoutMs: 10_000,
  });

  assertEquals(result.status.code, 0, `stderr: ${result.stderr}`);
  const calls = result.stdoutFrames.filter((f) => "call" in f);
  // Exactly 25 calls reach the parent; the 26th never makes it out.
  assertEquals(calls.length, 25);
  const finals = result.stdoutFrames.filter((f) => "final" in f);
  assertEquals(finals.length, 1);
  const fin = finals[0].final as { succeeded: number; caught: string };
  assertEquals(fin.succeeded, 25);
  assertStringIncludes(fin.caught, "call_budget_exceeded");
});

// ---------------------------------------------------------------------------
// Acceptance: isolation — fetch("http://example.com") errors out under
// default-deny permissions.
// ---------------------------------------------------------------------------
Deno.test("isolation: fetch(http://example.com) is blocked by Deno permissions", async () => {
  const result = await driveRunner({
    header: {
      program: 'async function main(){ await fetch("http://example.com"); } main();',
      manifest: [],
    },
    onFrame: () => undefined,
    testTimeoutMs: 10_000,
  });

  assertEquals(result.status.code, 1, `stderr: ${result.stderr}`);
  const errFrames = result.stdoutFrames.filter((f) => "error" in f);
  assertEquals(errFrames.length, 1);
  const err = errFrames[0].error as { message: string };
  // Deno's error message varies slightly across versions; assert on the
  // canonical "net access" phrasing.
  assertStringIncludes(err.message.toLowerCase(), "net access");
});

// ---------------------------------------------------------------------------
// Extra: nested manifest names produce a working Proxy tree.
// ---------------------------------------------------------------------------
Deno.test("nested manifest: tools.wiki.draftHint resolves dotted leaf", async () => {
  const program = `
    async function main(){
      const hint = await tools.wiki.draftHint({ from: "x" });
      return hint;
    }
    main();
  `;
  const result = await driveRunner({
    header: { program, manifest: ["wiki.draftHint"] },
    onFrame: (f) => {
      if ("call" in f) return { id: f.id, result: "hello" };
      return undefined;
    },
    testTimeoutMs: 10_000,
  });

  assertEquals(result.status.code, 0, `stderr: ${result.stderr}`);
  const calls = result.stdoutFrames.filter((f) => "call" in f);
  assertEquals(calls.length, 1);
  assertEquals(calls[0].call, "wiki.draftHint");
  const finals = result.stdoutFrames.filter((f) => "final" in f);
  assertEquals(finals[0].final, "hello");
});

// ---------------------------------------------------------------------------
// Extra: tools that aren't in the manifest produce a `tool_not_in_manifest`
// error inside the program — they never reach the parent.
// ---------------------------------------------------------------------------
Deno.test("manifest enforcement: missing tool raises in-program error", async () => {
  const program = `
    async function main(){
      try { await tools.notAThing("x"); return "should-not-reach"; }
      catch (e) { return (e && e.message) || String(e); }
    }
    main();
  `;
  const result = await driveRunner({
    header: { program, manifest: ["draft"] },
    onFrame: (f) => {
      if ("call" in f) return { id: f.id, result: null };
      return undefined;
    },
    testTimeoutMs: 10_000,
  });

  assertEquals(result.status.code, 0, `stderr: ${result.stderr}`);
  // No call should have been emitted at all.
  const calls = result.stdoutFrames.filter((f) => "call" in f);
  assertEquals(calls.length, 0);
  const finals = result.stdoutFrames.filter((f) => "final" in f);
  assertStringIncludes(String(finals[0].final), "tool_not_in_manifest");
});

// ---------------------------------------------------------------------------
// Ordering: boot-line is fully parsed before eval(program) runs.
//
// The program calls tools.draft() as its very first statement (synchronously
// inside main()). For the runner to emit that RPC frame the boot-line must
// already have been consumed and the manifest installed — if eval happened
// before readFirstLine() returned, globalThis.tools would be undefined and
// the runner would crash before emitting any call frame.
// ---------------------------------------------------------------------------
Deno.test("ordering: boot-line fully parsed before eval(program) runs", async () => {
  // The program calls tools.draft() synchronously on first microtask tick.
  // If the boot line weren't fully drained first, `tools` would be undefined
  // and we'd get an error frame (or no call frame) instead.
  const result = await driveRunner({
    header: {
      program:
        'async function main(){ const r = await tools.draft("x","y","z"); return r; } main();',
      manifest: ["draft"],
    },
    onFrame: (f) => {
      if ("call" in f) return { id: f.id, result: "boot-order-ok" };
      return undefined;
    },
    testTimeoutMs: 10_000,
  });

  assertEquals(result.status.code, 0, `stderr: ${result.stderr}`);
  const calls = result.stdoutFrames.filter((f) => "call" in f);
  const finals = result.stdoutFrames.filter((f) => "final" in f);
  // A call frame must have arrived — proving tools was installed (boot line
  // parsed) before eval fired.
  assertEquals(calls.length, 1, "expected exactly one call frame");
  assertEquals(calls[0].call, "draft");
  // The first frame in the sequence must be the call, not an error.
  assertEquals(
    "call" in result.stdoutFrames[0],
    true,
    "first emitted frame must be a call (not an error), confirming boot-line was parsed first",
  );
  assertEquals(finals.length, 1);
  assertEquals(finals[0].final, "boot-order-ok");
});

// ---------------------------------------------------------------------------
// Race regression: rpc() registers the pending Promise BEFORE writing the
// call frame to stdout, so a pre-buffered response arriving in the same stdin
// chunk as the boot line is still matched correctly.
//
// We write boot-line + RPC response {"id":1,"result":"ok"} in a single
// write() call (one chunk). If the pending-Promise registration ever moved to
// after writeLine(), the reader loop could consume the response before the
// Promise was registered and the runner would hang forever on `await rpc()`.
// ---------------------------------------------------------------------------
Deno.test({
  name: "rpc race: pre-buffered response in same stdin chunk resolves correctly",
  sanitizeResources: false,
  fn: async () => {
    const header = {
      program: 'async function main(){ return await tools.ping(); } main();',
      manifest: ["ping"],
    };
    // We spawn manually so we can write both lines in a single chunk.
    const cmd = new Deno.Command(Deno.execPath(), {
      args: ["run", RUNNER],
      stdin: "piped",
      stdout: "piped",
      stderr: "piped",
    });
    const child = cmd.spawn();
    const writer = child.stdin.getWriter();

    // Single write: boot line + pre-arranged response for id=1.
    // The runner must match this response even though it arrives in the same
    // read() call as the boot line (i.e. before rpc() has written the call
    // frame to stdout).
    const combined =
      JSON.stringify(header) + "\n" + JSON.stringify({ id: 1, result: "ok" }) + "\n";
    await writer.write(new TextEncoder().encode(combined));
    // Close stdin immediately — no further writes needed.
    try {
      await writer.close();
    } catch { /* ignore */ }

    // Collect all stdout frames.
    const stdoutFrames: Frame[] = [];
    const reader = child.stdout.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    const deadline = Date.now() + 10_000;
    while (true) {
      if (Date.now() > deadline) {
        try {
          child.kill("SIGKILL");
        } catch { /* already dead */ }
        throw new Error("test wall clock exceeded");
      }
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value);
      let idx;
      while ((idx = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, idx);
        buf = buf.slice(idx + 1);
        if (line.length === 0) continue;
        stdoutFrames.push(JSON.parse(line) as Frame);
      }
    }
    reader.releaseLock();

    const stderrBytes = await new Response(child.stderr).bytes();
    const status = await child.status;

    assertEquals(
      status.code,
      0,
      `runner exited non-zero — pre-buffered response was likely dropped.\nstderr: ${new TextDecoder().decode(stderrBytes)}`,
    );
    const calls = stdoutFrames.filter((f) => "call" in f);
    const finals = stdoutFrames.filter((f) => "final" in f);
    assertEquals(calls.length, 1, "expected one call frame");
    assertEquals(calls[0].call, "ping");
    assertEquals(finals.length, 1, "expected one final frame");
    // The resolved value from the pre-buffered response must propagate.
    assertEquals(finals[0].final, "ok");
  },
});

// ---------------------------------------------------------------------------
// Extra: RPC error frames from the parent surface as in-program throws.
// ---------------------------------------------------------------------------
Deno.test("rpc error: parent {id,error} frame rejects the in-program await", async () => {
  const program = `
    async function main(){
      try { await tools.draft("gmail","hi","r"); return "should-not-reach"; }
      catch (e) { return (e && e.message) || String(e); }
    }
    main();
  `;
  const result = await driveRunner({
    header: { program, manifest: ["draft"] },
    onFrame: (f) => {
      if ("call" in f) return { id: f.id, error: "rate_limited" };
      return undefined;
    },
    testTimeoutMs: 10_000,
  });

  assertEquals(result.status.code, 0, `stderr: ${result.stderr}`);
  const finals = result.stdoutFrames.filter((f) => "final" in f);
  assertStringIncludes(String(finals[0].final), "rate_limited");
});
