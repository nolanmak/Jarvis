import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium } from "playwright";
import { BrowserSession } from "../browser.mjs";
const html = `<!doctype html><title>Flight fixture</title><body><h1>Find flights</h1>
<input aria-label="Departure date" id="date"><button onclick="fetch('/search',{method:'POST'}).then(r=>r.json()).then(r=>document.getElementById('result').textContent='SFO to PHL '+document.getElementById('date').value+' One way 1 adult Economy USD '+r.price+' total, 1 stop, baggage unknown')">Search</button>
<button onclick="document.body.textContent='BOUGHT'">Buy ticket</button><p id="result"></p></body>`;
test("interactive flight fixture fills dates and observes POST-rendered fares while preserving owner tabs", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "general-browser-"));
  const context = await chromium.launchPersistentContext(dir, {
    channel: "chrome",
    headless: true,
    args: ["--remote-debugging-port=0"],
  });
  const unrelated = context.pages()[0];
  let posts = 0;
  const session = new BrowserSession({
    profileDirectory: dir,
    lockDirectory: dir,
    forward: async (req) => {
      if (new URL(req.url()).pathname === "/login")
        return {
          status: 200,
          contentType: "text/html",
          body: '<h1>Sign in</h1><input type="password">',
        };
      if (new URL(req.url()).pathname === "/captcha")
        return {
          status: 200,
          contentType: "text/html",
          body: '<h1>Verify</h1><iframe src="https://fixture.test/recaptcha"></iframe>',
        };
      if (new URL(req.url()).pathname === "/search") {
        posts++;
        return {
          status: 200,
          contentType: "application/json",
          body: '{"price":187}',
        };
      }
      return { status: 200, contentType: "text/html", body: html };
    },
  });
  t.after(async () => {
    await session.close();
    await context.close();
    await rm(dir, { recursive: true, force: true });
  });
  await session.open(["fixture.test"]);
  let o = await session.act(
    { kind: "navigate", url: "https://fixture.test" },
    { actions: 0 },
  );
  const field = o.elements.find((e) => e.label === "Departure date");
  assert.ok(field, "rendered form is visible");
  assert.equal(
    await session.page.evaluate(() => typeof RTCPeerConnection),
    "undefined",
  );
  o = await session.act(
    { kind: "type", ref: field.ref, text: "2026-09-24" },
    { actions: 1 },
  );
  const search = o.elements.find((e) => e.label === "Search");
  o = await session.act({ kind: "click", ref: search.ref }, { actions: 2 });
  assert.match(o.text, /SFO to PHL 2026-09-24.*USD 187/);
  assert.equal(posts, 1);
  assert.ok(o.screenshot.length > 100);
  const buy = o.elements.find((e) => e.label === "Buy ticket");
  await assert.rejects(
    session.act({ kind: "click", ref: buy.ref }, { actions: 3 }),
    /consequential/,
  );
  await session.page.getByText("Buy ticket", { exact: true }).focus();
  await assert.rejects(
    session.act({ kind: "press", key: "Enter" }, { actions: 3 }),
    /consequential/,
  );
  await assert.rejects(
    session.act({ kind: "click", ref: search.ref }, { actions: 4 }),
    /stale/,
  );
  o = await session.act(
    { kind: "navigate", url: "https://fixture.test/login" },
    { actions: 4 },
  );
  assert.equal(o.challenge, "login_required");
  o = await session.act(
    { kind: "navigate", url: "https://fixture.test/captcha" },
    { actions: 5 },
  );
  assert.equal(o.challenge, "captcha_required");
  await session.act(
    { kind: "navigate", url: "https://fixture.test" },
    { actions: 6 },
  );
  await session.page.locator("input").click();
  await session.page.waitForTimeout(50);
  await assert.rejects(
    session.act({ kind: "snapshot" }, { actions: 4 }),
    /owner_interference/,
  );
  await session.close();
  assert.equal(unrelated.isClosed(), false);
  assert.equal(context.pages().length, 1);
});
test("cancel while connecting never leaves a task tab or browser lock", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "cancel-browser-"));
  const context = await chromium.launchPersistentContext(dir, {
    channel: "chrome",
    headless: true,
    args: ["--remote-debugging-port=0"],
  });
  const session = new BrowserSession({
    profileDirectory: dir,
    lockDirectory: dir,
  });
  t.after(async () => {
    await context.close();
    await rm(dir, { recursive: true, force: true });
  });
  const opening = session.open(["example.com"]);
  const closing = session.close();
  await Promise.allSettled([opening, closing]);
  assert.equal(context.pages().length, 1);
  assert.equal(session.browser?.isConnected() ?? false, false);
});
