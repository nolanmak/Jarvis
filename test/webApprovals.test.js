// #2719 — Jarvis Dashboard approvals rail (spec §5.2.2, §5.4 approval card).
// Render tests with fixture actions and every outcome rendering.

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const express = require("express");
const { initDb, getDb } = require("../dist/db");
const { ReadDb } = require("../dist/web/readDb");
const { createFragmentRoutes } = require("../dist/web/fragments");

const tmpRoot = os.platform() === "darwin" ? "/tmp" : os.tmpdir();
const dir = fs.mkdtempSync(path.join(tmpRoot, "wp-"));
const dbPath = path.join(dir, "data.db");
initDb(dbPath);
test.after(() => fs.rmSync(dir, { recursive: true, force: true }));

{
  const db = getDb();
  const now = Date.now();
  const email = db.prepare(
    "INSERT INTO emails(messageId,fromEmail,subject,firstSeenAt,platform,kind) VALUES (?,?,?,?,?,?)",
  );
  const action = db.prepare(
    `INSERT INTO actions(id,messageId,fromEmail,recipientEmail,subject,draftBody,status,createdAt,updatedAt)
     VALUES (?,?,?,?,?,?,?,?,?)`,
  );
  email.run("m1", "Riley <riley@example.com>", "Renewal terms", now, "gmail", "dm");
  email.run("m2", "casey#1234", "Lunch Friday?", now, "discord", "dm");
  email.run("m3", "ops@example.com", "Old thread", now, "gmail", "dm");
  action.run("a-old", "m1", "Riley <riley@example.com>", "riley@vendor.example", "Re: Renewal terms",
    "Hi Riley,\nWe can accept the price <b>if</b> priority support stays.", "pending", now - 60_000, now - 60_000);
  action.run("a-new", "m2", "casey#1234", null, "Lunch Friday?", "Friday works, see you at noon.", "pending", now, now);
  action.run("a-done", "m3", "ops@example.com", null, "Old thread", "Done already", "sent", now, now);
}

async function fakeDaemon(t, reply) {
  const sockDir = fs.mkdtempSync(path.join(tmpRoot, "wd-"));
  const socketPath = path.join(sockDir, "web.sock");
  const requests = [];
  const server = net.createServer((conn) => {
    let buf = "";
    conn.on("data", (chunk) => {
      buf += chunk;
      const nl = buf.indexOf("\n");
      if (nl < 0) return;
      const req = JSON.parse(buf.slice(0, nl));
      requests.push(req);
      conn.end(JSON.stringify(reply(req)) + "\n");
    });
  });
  await new Promise((r) => server.listen(socketPath, r));
  fs.chmodSync(socketPath, 0o600);
  process.env.AUGMENTAGENT_WEB_SOCKET = socketPath;
  t.after(() => {
    delete process.env.AUGMENTAGENT_WEB_SOCKET;
    server.close();
    fs.rmSync(sockDir, { recursive: true, force: true });
  });
  return requests;
}

async function startApp(t) {
  const readDb = new ReadDb(dbPath);
  const app = express();
  app.use(express.urlencoded({ extended: true }));
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "..", "views"));
  app.use(createFragmentRoutes({ readDb: () => readDb }));
  const server = app.listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  t.after(() => {
    server.close();
    readDb.close();
  });
  const base = `http://127.0.0.1:${server.address().port}`;
  return async (method, url, form) => {
    const res = await fetch(base + url, {
      method,
      headers: form ? { "content-type": "application/x-www-form-urlencoded" } : {},
      body: form ? new URLSearchParams(form).toString() : undefined,
    });
    return { status: res.status, headers: res.headers, html: await res.text() };
  };
}

test("the rail lists pending actions newest first with escaped previews and the three verbs", async (t) => {
  const call = await startApp(t);
  const res = await call("GET", "/web/approvals");
  assert.equal(res.status, 200);
  const html = res.html;
  assert.match(html, /^<div id="approvals"[^>]*hx-get="\/web\/approvals"[^>]*hx-trigger="every 20s, jarvis:refresh"/);
  assert.ok(html.indexOf('data-action-id="a-new"') < html.indexOf('data-action-id="a-old"'), "newest first");
  assert.ok(!html.includes("a-done"), "only pending actions");
  // Recipient override wins over the sender; the kind is shown.
  assert.match(html, /riley@vendor\.example/);
  assert.match(html, /Discord/);
  // Previews are escaped plain text with line breaks, never markdown or HTML.
  assert.ok(html.includes("Hi Riley,<br>We can accept the price &lt;b&gt;if&lt;/b&gt; priority support stays."));
  assert.ok(!html.includes("<b>if</b>"));
  const card = html.slice(html.indexOf('data-action-id="a-old"'));
  assert.match(card, /hx-post="\/web\/approvals\/a-old"/);
  for (const verb of ["approve", "skip", "revise"]) assert.match(card, new RegExp(`name="verb" value="${verb}"`));
  assert.match(card, /<textarea[^>]*name="note"/);
  // The count badge updates out of band.
  assert.match(html, /id="approvals-count"[^>]*hx-swap-oob="true"[^>]*>2</);
});

test("the app shell renders the rail and the approvals count in the bottom bar", async (t) => {
  const call = await startApp(t);
  const html = (await call("GET", "/")).html;
  assert.match(html, /<div id="approvals"/);
  assert.match(html, /<nav class="j-tabbar"[\s\S]*id="approvals-count"[^>]*>2</);
  assert.ok(html.includes('<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">'));
});

const OUTCOMES = {
  approved: { kind: "approved", summary: "Sent to riley@vendor.example." },
  revised: { kind: "revised", summary: "Revised; a new draft is pending." },
  scheduled: { kind: "scheduled", summary: "Scheduled.", local: "Mon 9:00 AM" },
  failed: { kind: "failed", summary: "Send failed.", message: "SMTP 550 mailbox unavailable" },
  manual_send_ready: { kind: "manual_send_ready", summary: "Ready to send by hand.", text: "Friday <works>, see you", link: "https://discord.com/channels/@me/123" },
};

test("resolving renders the outcome summary, with failed and manual_send_ready special-cased", async (t) => {
  let outcome = OUTCOMES.approved;
  const requests = await fakeDaemon(t, () => ({ ok: true, outcome }));
  const call = await startApp(t);

  for (const [kind, o] of Object.entries(OUTCOMES)) {
    outcome = o;
    const res = await call("POST", "/web/approvals/a-old", { verb: "approve", note: "" });
    assert.equal(res.status, 200, kind);
    assert.match(res.html, /^<div id="approvals"/, kind);
    const oob = res.html.slice(res.html.indexOf('hx-swap-oob="afterbegin:#approval-outcomes"'));
    assert.ok(oob.length > 40, `${kind} outcome swapped in out of band`);
    assert.ok(oob.includes(o.summary.replace("<", "&lt;")), `${kind} summary`);
    assert.match(oob, new RegExp(`j-outcome-${kind}`));
  }
  // An empty note is not sent with approve.
  assert.deepEqual(requests[0], { v: 1, op: "action.resolve", action_id: "a-old", verb: "approve" });

  outcome = OUTCOMES.failed;
  const failed = (await call("POST", "/web/approvals/a-old", { verb: "approve" })).html;
  assert.match(failed, /SMTP 550 mailbox unavailable/);

  outcome = OUTCOMES.scheduled;
  assert.match((await call("POST", "/web/approvals/a-old", { verb: "approve" })).html, /Mon 9:00 AM/);

  outcome = OUTCOMES.manual_send_ready;
  const manual = (await call("POST", "/web/approvals/a-old", { verb: "approve" })).html;
  assert.match(manual, /Not sent: send this yourself/);
  assert.match(manual, /<textarea[^>]*readonly[^>]*>Friday &lt;works&gt;, see you<\/textarea>/);
  assert.match(manual, /data-action="copy"/);
  assert.match(manual, /href="https:\/\/discord\.com\/channels\/@me\/123"[^>]*rel="noopener noreferrer"/);

  outcome = { ...OUTCOMES.manual_send_ready, link: "javascript:alert(1)" };
  const unsafe = (await call("POST", "/web/approvals/a-old", { verb: "approve" })).html;
  assert.ok(!unsafe.includes("javascript:"), "unsafe links are dropped");

  outcome = OUTCOMES.revised;
  await call("POST", "/web/approvals/a-old", { verb: "revise", note: "Shorter please" });
  assert.deepEqual(requests.at(-1), { v: 1, op: "action.resolve", action_id: "a-old", verb: "revise", note: "Shorter please" });
});

test("resolve errors render in the toast with the mapped status", async (t) => {
  await fakeDaemon(t, () => ({ ok: false, code: "already_resolved", error: "already" }));
  const call = await startApp(t);
  const res = await call("POST", "/web/approvals/a-old", { verb: "skip" });
  assert.equal(res.status, 409);
  assert.equal(res.headers.get("hx-retarget"), "#j-toast");
  assert.match(res.html, /Already resolved elsewhere/);
  const bad = await call("POST", "/web/approvals/a-old", { verb: "revise", note: "  " });
  assert.equal(bad.status, 400);
  assert.match(bad.html, /revise needs a note/);
});
