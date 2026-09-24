import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium } from "playwright";
import {
  BrowserSession,
  RESPONSE_BODY_CAP,
  RESPONSE_ENTRY_CAP,
  RESPONSE_TOTAL_CAP,
} from "../browser.mjs";
const html = `<!doctype html><title>Fare fixture</title><body><h1>Fares</h1>
<button id="allowed" onclick="fetch('/search?token=SECRET-TOKEN&key=SECRET-KEY',{method:'POST'}).then(r=>r.json()).then(r=>document.getElementById('result').textContent='USD '+r.price)">Search</button>
<button id="big" onclick="fetch('/big').then(r=>r.text()).then(t=>document.getElementById('result').textContent='len '+t.length)">Big</button>
<button id="multibyte" onclick="fetch('/multibyte').then(r=>r.text())">Multibyte</button>
<button id="error" onclick="fetch('/error').catch(()=>{})">Error</button>
<button id="types" onclick="(async()=>{await fetch('/image');await fetch('/page')})()">Types</button>
<button id="many" onclick="(async()=>{for(let i=1;i<=5;i++)await fetch('/small/'+i)})()">Many</button>
<button id="heavy" onclick="(async()=>{for(let i=1;i<=3;i++)await fetch('/heavy/'+i)})()">Heavy</button>
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
          // Media types are case-insensitive; parse JSON regardless of case.
          contentType: "Application/JSON; charset=UTF-8",
          headers: {
            "set-cookie": "sid=abc",
            authorization: "Bearer secret-tok",
          },
          body: '{"price":187}',
        };
      if (u.pathname === "/big")
        return {
          status: 200,
          headers: { "content-type": "text/plain" },
          body: Buffer.from("x".repeat(RESPONSE_BODY_CAP + 2048)),
        };
      if (u.pathname === "/multibyte")
        return {
          status: 200,
          contentType: "text/plain; charset=utf-8",
          // Two-byte "é" straddles the cap boundary.
          body: Buffer.from("x".repeat(RESPONSE_BODY_CAP - 1) + "é"),
        };
      if (u.pathname === "/error")
        return {
          status: 500,
          contentType: "application/json",
          body: '{"error":"boom"}',
        };
      if (u.pathname === "/image")
        return {
          status: 200,
          contentType: "image/png",
          body: Buffer.from([0x89, 0x50, 0x4e, 0x47, 0xff, 0xfe, 0x00]),
        };
      if (u.pathname === "/page")
        return {
          status: 200,
          contentType: "text/html",
          body: "<p>fragment</p>",
        };
      if (u.pathname.startsWith("/small/"))
        return {
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({ n: Number(u.pathname.slice(7)) }),
        };
      if (u.pathname.startsWith("/heavy/"))
        return {
          status: 200,
          contentType: "text/plain",
          body: Buffer.from(u.pathname.slice(7).repeat(RESPONSE_BODY_CAP)),
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
  const click = async (label, actions) => {
    const el = o.elements.find((e) => e.label === label);
    assert.ok(el, `button ${label} is visible`);
    return session.act({ kind: "click", ref: el.ref }, { actions });
  };

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

  // Allowed-host XHR body is captured as structured JSON, with the media type
  // matched case-insensitively.
  o = await click("Search", 1);
  assert.equal(o.networkResponses.length, 1);
  const [searchEntry] = o.networkResponses;
  assert.equal(searchEntry.host, "fixture.test");
  assert.equal(searchEntry.path, "/search");
  assert.equal(searchEntry.status, 200);
  assert.equal(searchEntry.contentType, "application/json; charset=utf-8");
  assert.deepEqual(searchEntry.json, { price: 187 });
  assert.deepEqual(Object.keys(searchEntry).sort(), [
    "contentType",
    "host",
    "json",
    "observedAt",
    "path",
    "status",
  ]);

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

  // Per-action evidence: a following action that issues no requests does not
  // carry the previous action's responses.
  o = await session.act({ kind: "snapshot" }, { actions: 2 });
  assert.deepEqual(o.networkResponses, []);

  // Byte cap: an oversized body is truncated to the declared cap constant.
  o = await click("Big", 3);
  const bigEntry = o.networkResponses.find((e) => e.path === "/big");
  assert.ok(bigEntry, "captured the oversized body");
  assert.equal(bigEntry.contentType, "text/plain");
  assert.equal(bigEntry.bodyExcerpt.length, RESPONSE_BODY_CAP);
  assert.ok(Buffer.byteLength(bigEntry.bodyExcerpt) <= RESPONSE_BODY_CAP);

  // Byte cap on a UTF-8 boundary: the partial trailing sequence is dropped
  // rather than decoded to U+FFFD, so the excerpt stays within the cap.
  o = await click("Multibyte", 4);
  const multibyteEntry = o.networkResponses.find(
    (e) => e.path === "/multibyte",
  );
  assert.ok(multibyteEntry, "captured the multibyte body");
  assert.ok(Buffer.byteLength(multibyteEntry.bodyExcerpt) <= RESPONSE_BODY_CAP);
  assert.ok(!multibyteEntry.bodyExcerpt.includes("�"));
  assert.equal(multibyteEntry.bodyExcerpt, "x".repeat(RESPONSE_BODY_CAP - 1));

  // Only successful responses are evidence: a 500 body is not captured.
  o = await click("Error", 5);
  assert.deepEqual(o.networkResponses, []);

  // Only text/* or JSON media types are captured; binary bodies are skipped.
  o = await click("Types", 6);
  assert.deepEqual(
    o.networkResponses.map((e) => e.path),
    ["/page"],
  );
  assert.equal(o.networkResponses[0].bodyExcerpt, "<p>fragment</p>");

  // Entry cap: only the most recent RESPONSE_ENTRY_CAP responses are kept.
  o = await click("Many", 7);
  assert.equal(RESPONSE_ENTRY_CAP, 4);
  assert.deepEqual(
    o.networkResponses.map((e) => e.path),
    ["/small/2", "/small/3", "/small/4", "/small/5"],
  );

  // Aggregate byte cap: older bodies are dropped until the total fits.
  o = await click("Heavy", 8);
  assert.equal(RESPONSE_TOTAL_CAP, 128 * 1024);
  assert.deepEqual(
    o.networkResponses.map((e) => e.path),
    ["/heavy/2", "/heavy/3"],
  );
  assert.ok(
    o.networkResponses.reduce(
      (n, e) => n + Buffer.byteLength(e.bodyExcerpt),
      0,
    ) <= RESPONSE_TOTAL_CAP,
  );

  // Foreign host: a successful fetch to a host outside task.hosts is dropped.
  o = await click("Foreign", 9);
  assert.deepEqual(o.networkResponses, []);
});
