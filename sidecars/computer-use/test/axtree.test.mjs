import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium } from "playwright";
import { BrowserSession } from "../browser.mjs";

// Serve one synthetic document over the forward stub, navigate to it, and return
// the resulting snapshot so each fixture can assert on `o.elements`.
//
// This launches Chrome once, here, as a persistent debugging context (mirroring
// the existing browser.test.mjs harness). BrowserSession does NOT start a second
// browser: session.open() -> attach() reads the DevToolsActivePort this launch
// wrote into `dir` and connectOverCDP's onto this same running instance — the
// production "attach to the owner's already-running Chrome" model. So there is
// no second launch contending for the profile lock; `dir` is shared by design.
async function navigate(t, body) {
  const dir = await mkdtemp(join(tmpdir(), "axtree-browser-"));
  const context = await chromium.launchPersistentContext(dir, {
    channel: "chrome",
    headless: true,
    args: ["--remote-debugging-port=0"],
  });
  const session = new BrowserSession({
    profileDirectory: dir,
    lockDirectory: dir,
    forward: async () => ({
      status: 200,
      contentType: "text/html",
      body: `<!doctype html><title>axtree fixture</title><body>${body}</body>`,
    }),
  });
  t.after(async () => {
    await session.close();
    await context.close();
    await rm(dir, { recursive: true, force: true });
  });
  await session.open(["fixture.test"]);
  const o = await session.act(
    { kind: "navigate", url: "https://fixture.test" },
    { actions: 0 },
  );
  return { session, o };
}

test("an input named only by <label for> emits the computed accessible name", async (t) => {
  const { o } = await navigate(
    t,
    `<label for="dep">Departure date</label><input id="dep">`,
  );
  const input = o.elements.find((e) => e.tag === "INPUT");
  assert.ok(input, "input is emitted");
  assert.equal(input.label, "Departure date");
});

test("an <a href> with no literal role emits the implicit link role", async (t) => {
  const { o } = await navigate(t, `<a href="/x">Book</a>`);
  const link = o.elements.find((e) => e.tag === "A");
  assert.ok(link, "anchor is emitted");
  assert.equal(link.role, "link");
});

test("an input named by aria-labelledby emits the referenced node's text", async (t) => {
  const { o } = await navigate(
    t,
    `<span id="lbl">Return date</span><input aria-labelledby="lbl">`,
  );
  const input = o.elements.find((e) => e.tag === "INPUT");
  assert.ok(input, "input is emitted");
  assert.equal(input.label, "Return date");
});

test("an aria-labelled button keeps a resolvable data-jarvis-ref and clicks", async (t) => {
  const { session, o } = await navigate(
    t,
    `<button aria-label="Search" onclick="document.getElementById('out').textContent='CLICKED'">Go</button><p id="out"></p>`,
  );
  const button = o.elements.find((e) => e.tag === "BUTTON");
  assert.ok(button, "button is emitted");
  assert.match(button.ref, /-\d+$/);
  assert.equal(button.label, "Search");
  assert.equal(
    await session.page.locator(`[data-jarvis-ref="${button.ref}"]`).count(),
    1,
    "ref resolves to exactly one live locator",
  );
  const after = await session.act(
    { kind: "click", ref: button.ref },
    { actions: 1 },
  );
  assert.match(after.text, /CLICKED/);
});

test("enrichment keeps the emitted element count within the 250 cap", async (t) => {
  const buttons = Array.from(
    { length: 300 },
    (_, i) => `<button>b${i}</button>`,
  ).join("");
  const { o } = await navigate(t, buttons);
  assert.ok(o.elements.length <= 250, `emitted ${o.elements.length} elements`);
});

// The reported case: an actionable element the fixed selector list never
// matches (<div role="link"> is not one of a/button/input/select/textarea or
// the enumerated roles) must still be surfaced from the AX tree, stamped, and
// clickable — grounding the set on the accessibility tree, not the scrape.
test("an actionable AX node the selector scrape misses is emitted and clickable", async (t) => {
  const { session, o } = await navigate(
    t,
    `<div role="link" onclick="document.getElementById('out').textContent='OPENED'">Flight details</div><p id="out"></p>`,
  );
  const link = o.elements.find((e) => e.role === "link");
  assert.ok(link, "AX-only actionable node is emitted");
  assert.equal(link.tag, "DIV");
  assert.equal(link.label, "Flight details");
  assert.match(link.ref, /-\d+$/);
  assert.equal(
    await session.page.locator(`[data-jarvis-ref="${link.ref}"]`).count(),
    1,
    "discovered ref resolves to exactly one live locator",
  );
  const after = await session.act(
    { kind: "click", ref: link.ref },
    { actions: 1 },
  );
  assert.match(after.text, /OPENED/);
});

// Regression for the repeat-snapshot drop: the stale data-jarvis-ref stamped on
// an AX-only node during the first snapshot must not mark it "covered" on the
// next snapshot (the scrape never re-emits it), or it silently vanishes from the
// grounded set and can no longer be clicked.
test("an AX-only actionable node survives a repeat snapshot", async (t) => {
  const { session, o } = await navigate(
    t,
    `<div role="link" onclick="document.getElementById('out').textContent='OPENED'">Flight details</div><p id="out"></p>`,
  );
  assert.ok(
    o.elements.find((e) => e.role === "link"),
    "AX-only node emitted on the first snapshot",
  );
  const o2 = await session.act({ kind: "snapshot" }, { actions: 1 });
  const link = o2.elements.find((e) => e.role === "link");
  assert.ok(link, "AX-only node still emitted on the repeat snapshot");
  assert.equal(link.label, "Flight details");
  assert.equal(
    await session.page.locator(`[data-jarvis-ref="${link.ref}"]`).count(),
    1,
    "repeat-snapshot ref resolves to exactly one live locator",
  );
  const after = await session.act(
    { kind: "click", ref: link.ref },
    { actions: 2 },
  );
  assert.match(after.text, /OPENED/);
});
