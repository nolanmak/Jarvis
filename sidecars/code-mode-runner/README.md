# AugmentAgent code-mode runner sidecar

Deno-based sandbox that runs LLM-generated TypeScript programs for AugmentAgent
Code Mode. The Rust daemon spawns `deno run runner.ts` (no `--allow-*` flags),
pipes the program in over stdin, and dispatches `tools.*` RPC calls back to the
backing Rust implementations.

Implements [#47](https://github.com/nolanmak/MyAgentAssistant/issues/47).
Part of the Code Mode for AugmentAgent epic.

## Layout

```
sidecars/code-mode-runner/
  runner.ts        # the sandbox runner (entry point)
  runner_test.ts   # acceptance tests (deno test)
  deno.json        # Deno config: imports locked empty, strict TS
  README.md        # this file
```

## Setup

The runner requires Deno >= 2.7 on the host. It is **not** vendored — install
once per box:

```bash
# One-liner installer (puts the binary at ~/.deno/bin/deno).
curl -fsSL https://deno.land/install.sh | sh

# Make it available on PATH for the daemon. Either:
#   - add `export PATH="$HOME/.deno/bin:$PATH"` to your shell rc, or
#   - symlink: sudo ln -s ~/.deno/bin/deno /usr/local/bin/deno

deno --version   # verify: deno 2.7.x or newer
```

The pinned Deno version lives in `deno.json`'s `_minDenoVersion` field.
The Rust caller (issue #50) is expected to check `deno --version` at startup
and refuse to boot if the binary is missing or too old.

## How the sandbox is locked down

The runner is invoked with **no `--allow-*` flags**, which in Deno 2 means
default-deny on every capability:

- No network (`fetch`, `Deno.connect`, `WebSocket`).
- No filesystem (`Deno.readFile`, `Deno.writeFile`, dynamic `import` from disk).
- No environment access (`Deno.env`).
- No subprocesses (`Deno.Command`).
- No FFI, no `--unstable` APIs.

Note: the original issue spec refers to `--allow-none`. That is **not** a real
Deno flag — passing it errors out (`unexpected argument '--allow-none' found`).
The semantic equivalent is simply omitting all `--allow-*` flags. The runner
is therefore launched as `deno run runner.ts`.

`deno.json` additionally locks down:

- `"imports": {}` — empty import map; the program text cannot resolve any
  bare specifier or remote URL.
- `"nodeModulesDir": "none"` — no npm shim.
- `"lock": false` — no remote dependency caching at runtime.

## Wire protocol

NDJSON, one JSON object per line, both directions over stdin/stdout. Stderr is
unused by the protocol (Deno may print warnings there; the Rust caller should
log them).

### Header (parent → runner, first line)

Exactly one line. Required before any other I/O.

```json
{"program": "<TypeScript source>", "manifest": ["wiki.draftHint", "draft", ...]}
```

- `program` is a string of TypeScript / JavaScript that the runner evaluates.
  The convention (and what the system prompt in issue #51 enforces) is:

  ```ts
  async function main(): Promise<void | unknown> { ... }
  main();
  ```

  The trailing `main();` is required: the runner uses indirect eval and awaits
  the value of the last expression statement, which is the call to `main`.
- `manifest` is a flat allowlist of tool names. Dotted names produce nested
  namespaces in `tools` — e.g. `"wiki.draftHint"` is exposed as
  `tools.wiki.draftHint`.

### Tool call (runner → parent)

Every time the program awaits a `tools.*` call, the runner emits:

```json
{"id": 1, "call": "wiki.draftHint", "args": [{"from": "alice@example.com"}]}
```

`id` is a monotonic integer scoped to the runner process.

### Tool result (parent → runner)

```json
{"id": 1, "result": "lead from MIT, prefers brevity"}
```

or

```json
{"id": 1, "error": "rate_limited: backoff 30s"}
```

The runner matches by `id` and resolves / rejects the in-program await.

### Final result (runner → parent, last line on success)

```json
{"final": <value returned by main(), or null>}
```

The runner then exits 0.

### Error frame (runner → parent, last line on failure)

```json
{"error": {"message": "...", "stack": "...", "kind": "timeout"}}
```

`kind` is set to `"timeout"` only when the 60s wall-clock kill fired. The
runner exits 1.

## Enforced limits

- **60s wall-clock** on `await main()`. The runner emits an error frame with
  `kind: "timeout"` and exits 1.
- **25 RPC calls per program.** The 26th `tools.*` call throws synchronously
  inside the program with `Error("call_budget_exceeded: ...")`. The program
  may catch and recover (e.g. return a partial result), but it cannot raise
  the cap.
- **Manifest enforcement.** Accessing a path not in the manifest throws
  `Error("tool_not_in_manifest: <path>")` inside the program — no RPC frame
  is emitted to the parent.

## End-to-end smoke

The shortest possible round-trip, hand-driven:

```bash
# In one terminal, prime two NDJSON lines and pipe them through the runner.
# First line is the header; second is the canned tool result.
printf '%s\n%s\n' \
  '{"program":"async function main(){ await tools.draft(\"gmail\",\"hi\",\"r\"); } main();","manifest":["draft"]}' \
  '{"id":1,"result":null}' \
| deno run runner.ts
```

Expected stdout (exactly two lines, exit 0):

```
{"id":1,"call":"draft","args":["gmail","hi","r"]}
{"final":null}
```

## Running the acceptance tests

```bash
# From this directory:
deno task test

# Or explicitly:
deno test --allow-run --allow-read=. --no-prompt runner_test.ts
```

The test harness needs `--allow-run` (to spawn the runner as a subprocess) and
`--allow-read=.` (so Deno can load `runner.ts`). The **runner subprocess
itself** is spawned with no `--allow-*` flags, so the tests exercise the same
isolation profile that production uses.

Coverage:

- Smoke: single `tools.draft(...)` round-trip, `final:null`, exit 0.
- Timeout: a 70s sleep is killed in under 61s with a timeout error frame.
- Budget: a 26-iteration loop sees the 26th call reject with
  `call_budget_exceeded`; only 25 call frames reach the parent.
- Isolation: `fetch("http://example.com")` errors out
  (`Requires net access ...`).
- Nested manifest: `tools.wiki.draftHint` resolves through the dotted path.
- Manifest enforcement: `tools.notAThing(...)` raises in-program without
  emitting an RPC frame.
- RPC error frames: parent-side `{id, error}` rejects the in-program await.

The timeout test takes ~60s by design; the rest are sub-second.

## Operational notes

- The runner has no persistent state — every invocation is a fresh sandbox.
  The Rust caller is responsible for capturing the `toolCallTrace` (sequence
  of `{call, args, result}` triples) for audit, since the runner only logs
  the wire frames.
- Stdout is **NDJSON only**. The runner never prints free-form text on stdout;
  any such text indicates a bug. Deno warnings (e.g. `"exports" field should
  be specified`) go to stderr and are safe to ignore at the protocol level.
- The runner does not support nested code-mode (`tools.codeMode(...)`) or
  streaming partials. Those are explicitly out-of-scope for v1 (see the epic).
- If the parent closes stdin while RPCs are still in flight, those awaits
  reject with `Error("stdin closed")`. The runner then propagates whatever
  the program does in response (catch + return, or uncaught throw → error
  frame).

## Troubleshooting

| Symptom | Cause / Fix |
|---|---|
| `unexpected argument '--allow-none' found` | You're passing the deprecated/never-real `--allow-none` flag. Use no `--allow-*` flag at all; default-deny is the default. |
| Runner hangs after emitting an RPC | The parent didn't reply with a matching `{id, result\|error}` on stdin, or replied with a different `id`. Match by integer `id`. |
| Program crashes with `tool_not_in_manifest` for a name you *did* allow | Manifest entries are dotted leaf names. To expose `tools.x.y` you must put `"x.y"` in the manifest, not `"x"`. |
| `{"error": {... "kind": "timeout"}}` | Program ran longer than 60s. Either shorten it or accept the failure; v1 has no per-program override of the cap. |
| Parent process never sees `{"final": ...}` | The program threw uncaught. Look at the preceding `{"error": ...}` frame. |
