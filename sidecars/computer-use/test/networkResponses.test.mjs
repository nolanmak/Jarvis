import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium } from "playwright";
import { BrowserSession, RESPONSE_BODY_CAP } from "../browser.mjs";
const html = `<!doctype html><title>Fare fixture</title><body><h1>Fares</h1>
<button id="allowed" onclick="fetch('/search?token=SECRET-TOKEN&key=SECRET-KEY',{method:'POST'}).then(r=>r.json()).then(r=>document.getElementById('result').textContent='USD '+r.price)">Search</button>
<button id="big" onclick="fetch('/big').then(r=>r.text()).then(t=>document.getElementById('result').textContent='len '+t.length)">Big</button>
<button id="foreign" onclick="fetch('https://other.test/search').then(r=>r.json()).catch(()=>{})">Foreign</button>
<p id="result"></p></body>`;
test("captures sanitized off-the-wire fare bodies for allowed hosts within the byte cap", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "netresp-browser-"));
  const context = await chromium.launchPersistentContext(dir, {
    channel: "chrome",
    headless: true,
    args: ["--remote-debugging-port=0"],
  });
  const session = new BrowserSession({
    profileDirectory: dir,
    lockDirectory: dir,
    forward: async (req) => {
      const u = new URL(req.url());
      if (u.pathname === "/search")
        return {
          status: 200,
          contentType: "application/json",
          headers: { "set-cookie": "sid=abc", authorization: "Bearer secret-tok" },
          body: '{"price":187}',
        };
      if (u.pathname === "/big")
        return {
          status: 200,
          headers: { "content-type": "text/plain" },
          body: Buffer.from("x".repeat(RESPONSE_BODY_CAP + 2048)),
        };
      return { status: 200, contentType: "text/html", body: html };
    },
  });
  t.after(async () => {
    await session.close();
    await context.close();
    await rm(dir, { recursive: true, force: true });
  });
  await session.open(["fixture.test"]);

  // Regression: a document-only load carries no captured responses and leaves
  // every pre-existing snapshot field intact.
  let o = await session.act(
    { kind: "navigate", url: "https://fixture.test" },
    { actions: 0 },
  );
  assert.deepEqual(o.networkResponses, []);
  assert.equal(o.title, "Fare fixture");
  assert.match(o.text, /Fares/);
  assert.ok(o.elements.some((e) => e.label === "Search"));
  assert.equal(o.challenge, null);
  assert.ok(o.screenshot.length > 100);
  assert.deepEqual(o.networkFailures, []);

  // Allowed-host XHR body is captured as structured JSON.
  const search = o.elements.find((e) => e.label === "Search");
  o = await session.act({ kind: "click", ref: search.ref }, { actions: 1 });
  const searchEntry = o.networkResponses.find((e) => e.path === "/search");
  assert.ok(searchEntry, "captured the allowed-host fetch body");
  assert.equal(searchEntry.host, "fixture.test");
  assert.equal(searchEntry.path, "/search");
  assert.equal(searchEntry.status, 200);
  assert.equal(searchEntry.contentType, "application/json");
  assert.deepEqual(searchEntry.json, { price: 187 });

  // Sanitization: no header names, cookie/authorization material, or credential
  // query parameters survive into the evidence.
  const serialized = JSON.stringify(o.networkResponses);
  for (const forbidden of [
    "set-cookie",
    "cookie",
    "authorization",
    "Bearer",
    "sid=abc",
    "secret-tok",
    "token=",
    "key=",
    "SECRET-TOKEN",
    "SECRET-KEY",
  ])
    assert.ok(!serialized.includes(forbidden), `evidence leaks ${forbidden}`);

  // Byte cap: an oversized body is truncated to the declared cap constant.
  const big = o.elements.find((e) => e.label === "Big");
  o = await session.act({ kind: "click", ref: big.ref }, { actions: 2 });
  const bigEntry = o.networkResponses.find((e) => e.path === "/big");
  assert.ok(bigEntry, "captured the oversized body");
  assert.equal(bigEntry.contentType, "text/plain");
  assert.equal(bigEntry.bodyExcerpt.length, RESPONSE_BODY_CAP);
  assert.ok(Buffer.byteLength(bigEntry.bodyExcerpt) <= RESPONSE_BODY_CAP);

  // Foreign host: a successful fetch to a host outside task.hosts is dropped.
  const foreign = o.elements.find((e) => e.label === "Foreign");
  o = await session.act({ kind: "click", ref: foreign.ref }, { actions: 3 });
  assert.ok(
    o.networkResponses.every((e) => e.host === "fixture.test"),
    "only allowed-host responses are captured",
  );
  assert.ok(!o.networkResponses.some((e) => e.host === "other.test"));
});
