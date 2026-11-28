// #2708 — Access JWT check inside requireAuth (spec D6, §5.3 auth, §6 item 4).
// Uses a locally generated RSA key and a stubbed JWKS endpoint; nothing here
// talks to the network.

const test = require("node:test");
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const TEAM = "test.cloudflareaccess.com";
const AUD = "test-aud";
const OWNER = "owner@example.com";
const API_KEY = "synthetic-dashboard-key";

process.env.MODE = "local";
process.env.AUGMENTAGENT_API_KEY = API_KEY;
process.env.GITHUB_WEBHOOK_SECRET = "ghsek";
process.env.LINEAR_WEBHOOK_SECRET = "linsek";
const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "aa-cfaccess-"));
process.env.AUGMENTAGENT_DB = path.join(tmpDir, "data.db");

const { verifyAccessJwt, JwksCache, JwksUnavailableError, accessConfigFromEnv } = require("../dist/web/cfAccess");
const security = require("../dist/security");
const db = require("../dist/db");
db.initDb(process.env.AUGMENTAGENT_DB);

function setAccessEnv() {
  process.env.AUGMENTAGENT_CF_ACCESS_AUD = AUD;
  process.env.AUGMENTAGENT_CF_ACCESS_TEAM_DOMAIN = TEAM;
  process.env.AUGMENTAGENT_CF_ACCESS_EMAILS = `${OWNER}, second@example.com`;
}
function clearAccessEnv() {
  delete process.env.AUGMENTAGENT_CF_ACCESS_AUD;
  delete process.env.AUGMENTAGENT_CF_ACCESS_TEAM_DOMAIN;
  delete process.env.AUGMENTAGENT_CF_ACCESS_EMAILS;
}

function keypair(kid) {
  const { privateKey, publicKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 });
  return { kid, privateKey, publicKey, jwk: { ...publicKey.export({ format: "jwk" }), kid, alg: "RS256", use: "sig" } };
}
const K1 = keypair("kid-1");
const K2 = keypair("kid-2");

const b64u = (v) => Buffer.from(typeof v === "string" ? v : JSON.stringify(v)).toString("base64url");
function sign(kp, claims = {}, header = {}) {
  const nowS = Math.floor(Date.now() / 1000);
  const h = b64u({ alg: "RS256", kid: kp.kid, typ: "JWT", ...header });
  const p = b64u({ aud: [AUD], iss: `https://${TEAM}`, email: OWNER, iat: nowS, nbf: nowS, exp: nowS + 600, ...claims });
  const sig = crypto.sign("RSA-SHA256", Buffer.from(`${h}.${p}`), kp.privateKey).toString("base64url");
  return `${h}.${p}.${sig}`;
}
const opts = (extra = {}) => ({ aud: AUD, teamDomain: TEAM, allowEmails: [OWNER], now: Date.now(), keys: { [K1.kid]: K1.publicKey }, ...extra });

// --- pure verifier -----------------------------------------------------------

test("valid token is accepted and yields the email", () => {
  assert.deepEqual(verifyAccessJwt(sign(K1), opts()), { ok: true, email: OWNER });
  assert.equal(verifyAccessJwt(sign(K1, { aud: AUD }), opts()).ok, true, "string aud");
  assert.equal(verifyAccessJwt(sign(K1, { email: "Owner@Example.com" }), opts()).ok, true, "email case");
});

test("wrong aud or iss is rejected", () => {
  assert.equal(verifyAccessJwt(sign(K1, { aud: ["other-aud"] }), opts()).ok, false);
  assert.equal(verifyAccessJwt(sign(K1, { iss: "https://evil.cloudflareaccess.com" }), opts()).ok, false);
});

test("expired and not-yet-valid tokens are rejected, with 60 s skew", () => {
  const nowS = Math.floor(Date.now() / 1000);
  assert.equal(verifyAccessJwt(sign(K1, { exp: nowS - 61 }), opts()).ok, false);
  assert.equal(verifyAccessJwt(sign(K1, { exp: nowS - 30 }), opts()).ok, true, "within skew");
  assert.equal(verifyAccessJwt(sign(K1, { nbf: nowS + 120 }), opts()).ok, false);
  assert.equal(verifyAccessJwt(sign(K1, { nbf: nowS + 30 }), opts()).ok, true, "within skew");
  assert.equal(verifyAccessJwt(sign(K1, { exp: undefined }), opts()).ok, false, "exp required");
});

test("email not on the allowlist is rejected", () => {
  assert.equal(verifyAccessJwt(sign(K1, { email: "intruder@example.com" }), opts()).ok, false);
  assert.equal(verifyAccessJwt(sign(K1, { email: undefined }), opts()).ok, false);
});

test("bad signature, unknown kid, non-RS256 and garbage are rejected", () => {
  const forged = sign(K1).split(".");
  forged[2] = sign(K2).split(".")[2];
  assert.equal(verifyAccessJwt(forged.join("."), opts()).ok, false);
  assert.equal(verifyAccessJwt(sign(K2), opts()).ok, false);
  assert.equal(verifyAccessJwt(sign(K1, {}, { alg: "none" }), opts()).ok, false);
  assert.equal(verifyAccessJwt(sign(K1, {}, { alg: "HS256" }), opts()).ok, false);
  assert.equal(verifyAccessJwt("not-a-jwt", opts()).ok, false);
  assert.equal(verifyAccessJwt("", opts()).ok, false);
});

// --- JWKS cache --------------------------------------------------------------

function jwksFetch(state) {
  return async (url) => {
    state.calls.push(String(url));
    if (state.down) throw new Error("network down");
    return { ok: true, status: 200, json: async () => ({ keys: state.keys }) };
  };
}

test("unknown kid triggers a refresh (at most once per 60 s) and then verifies", async () => {
  let t = 1_000_000;
  const state = { calls: [], keys: [K1.jwk], down: false };
  const cache = new JwksCache({ teamDomain: TEAM, fetch: jwksFetch(state), now: () => t });
  assert.ok((await cache.keysFor("kid-1"))["kid-1"]);
  assert.deepEqual(state.calls, [`https://${TEAM}/cdn-cgi/access/certs`]);
  await cache.keysFor("kid-1");
  assert.equal(state.calls.length, 1, "cached");

  state.keys = [K1.jwk, K2.jwk]; // key rotation
  t += 10_000;
  assert.equal((await cache.keysFor("kid-2"))["kid-2"], undefined, "throttled inside 60 s");
  assert.equal(state.calls.length, 1);
  t += 51_000;
  const keys = await cache.keysFor("kid-2");
  assert.equal(state.calls.length, 2, "refreshed on unknown kid");
  assert.equal(verifyAccessJwt(sign(K2), opts({ keys })).ok, true);
});

test("keys are held for 1 h; JWKS down with no matching key fails closed", async () => {
  let t = 5_000_000;
  const state = { calls: [], keys: [K1.jwk], down: false };
  const cache = new JwksCache({ teamDomain: TEAM, fetch: jwksFetch(state), now: () => t });
  await cache.keysFor("kid-1");
  state.down = true;
  t += 30 * 60_000;
  assert.ok((await cache.keysFor("kid-1"))["kid-1"], "cached key survives an outage");
  await assert.rejects(cache.keysFor("kid-2"), JwksUnavailableError);
  t += 31 * 60_000;
  await assert.rejects(cache.keysFor("kid-1"), JwksUnavailableError, "expired after 1 h");

  const cold = new JwksCache({ teamDomain: TEAM, fetch: jwksFetch({ calls: [], keys: [], down: true }), now: () => t });
  await assert.rejects(cold.keysFor("kid-1"), JwksUnavailableError);
});

// --- config ------------------------------------------------------------------

test("partial Access config refuses to start", () => {
  clearAccessEnv();
  assert.equal(accessConfigFromEnv(process.env), null);
  security.assertAccessConfig();
  for (const only of ["AUGMENTAGENT_CF_ACCESS_AUD", "AUGMENTAGENT_CF_ACCESS_TEAM_DOMAIN", "AUGMENTAGENT_CF_ACCESS_EMAILS"]) {
    clearAccessEnv();
    process.env[only] = "x";
    assert.throws(() => security.assertAccessConfig(), /AUGMENTAGENT_CF_ACCESS_/);
  }
  setAccessEnv();
  process.env.AUGMENTAGENT_CF_ACCESS_EMAILS = " , ";
  assert.throws(() => security.assertAccessConfig(), /AUGMENTAGENT_CF_ACCESS_EMAILS/);
  setAccessEnv();
  assert.deepEqual(accessConfigFromEnv(process.env), { aud: AUD, teamDomain: TEAM, allowEmails: [OWNER, "second@example.com"] });
  security.assertAccessConfig();
  clearAccessEnv();
});

// --- requireAuth in Access mode ----------------------------------------------

const jwks = { calls: [], keys: [K1.jwk], down: false };
const realFetch = globalThis.fetch;
function useAccessMode() {
  setAccessEnv();
  jwks.calls = [];
  jwks.keys = [K1.jwk];
  jwks.down = false;
  globalThis.fetch = jwksFetch(jwks);
  security.resetAccessJwksCache();
}
test.afterEach(() => {
  clearAccessEnv();
  globalThis.fetch = realFetch;
});
test.after(() => fs.rmSync(tmpDir, { recursive: true, force: true }));

// A tunnel-shaped request: loopback peer (cloudflared) but public Host + XFF.
function request(overrides = {}) {
  const headers = { host: "dash.example.com", "x-forwarded-for": "198.51.100.7", accept: "application/json", ...overrides.headers };
  for (const k of Object.keys(headers)) if (headers[k] === undefined) delete headers[k];
  return { method: "GET", path: "/", socket: { remoteAddress: "127.0.0.1" }, ...overrides, headers, header(name) { return headers[name.toLowerCase()]; } };
}
function run(req, handler = security.requireAuth) {
  return new Promise((resolve) => {
    const out = {};
    const res = {
      status(n) { out.status = n; return this; },
      type() { return this; },
      setHeader() { return this; },
      json(b) { out.body = b; resolve(out); return this; },
      send(b) { out.body = b; resolve(out); return this; },
      redirect(a, b) { out.status = typeof a === "number" ? a : 302; out.redirect = b ?? a; resolve(out); },
    };
    handler(req, res, () => { out.allowed = true; resolve(out); });
  });
}
const local = (h = {}) => request({ headers: { host: "localhost:3000", "x-forwarded-for": undefined, accept: "text/html", "sec-fetch-site": "none", "sec-fetch-mode": "navigate", "sec-fetch-dest": "document", ...h } });

test("Access mode: valid Cf-Access-Jwt-Assertion is admitted, bad ones are 401", async () => {
  useAccessMode();
  assert.equal((await run(request({ headers: { "cf-access-jwt-assertion": sign(K1) } }))).allowed, true);
  assert.deepEqual(jwks.calls, [`https://${TEAM}/cdn-cgi/access/certs`]);
  for (const bad of [sign(K1, { aud: ["nope"] }), sign(K1, { email: "intruder@example.com" }), sign(K1, { exp: 1 }), "garbage"]) {
    const out = await run(request({ headers: { "cf-access-jwt-assertion": bad } }));
    assert.equal(out.allowed, undefined);
    assert.equal(out.status, 401);
  }
});

test("Access mode: missing header is 401 for API/htmx/SSE and 302 to Access login for a navigation", async () => {
  useAccessMode();
  assert.equal((await run(request())).status, 401, "API");
  assert.equal((await run(request({ headers: { "hx-request": "true", "sec-fetch-mode": "cors" } }))).status, 401, "htmx");
  assert.equal((await run(request({ headers: { accept: "text/event-stream" } }))).status, 401, "SSE");
  const nav = await run(request({ headers: { accept: "text/html", "sec-fetch-mode": "navigate", "sec-fetch-dest": "document" } }));
  assert.equal(nav.status, 302);
  assert.equal(nav.redirect, `https://${TEAM}/cdn-cgi/access/login/dash.example.com`);
});

test("Access mode: API key is still admitted", async () => {
  useAccessMode();
  assert.equal((await run(request({ headers: { authorization: `Bearer ${API_KEY}` } }))).allowed, true);
  assert.equal((await run(request({ headers: { "x-api-key": API_KEY } }))).allowed, true);
  assert.equal((await run(request({ headers: { authorization: "Bearer wrong" } }))).status, 401);
});

test("Access mode: loopback admit works for a loopback peer without forwarded headers and refuses tunnel-shaped requests", async () => {
  useAccessMode();
  assert.equal((await run(local())).allowed, true);
  assert.equal((await run(local({ "x-forwarded-for": "127.0.0.1" }))).allowed, undefined);
  assert.equal((await run(local({ host: "dash.example.com" }))).allowed, undefined);
  assert.equal((await run(request({ socket: { remoteAddress: "192.0.2.10" }, headers: { host: "localhost:3000", "x-forwarded-for": undefined } }))).allowed, undefined);
});

test("Access mode: cookie login is refused", async () => {
  // Mint a real session cookie with Access off, then turn Access on.
  clearAccessEnv();
  let cookie;
  const res = { setHeader(_n, v) { cookie = v.split(";")[0]; }, redirect() {}, status() { return this; }, type() { return this; }, send() { return this; } };
  security.loginSubmitHandler({ body: { key: API_KEY } }, res);
  assert.match(cookie, /^augmentagent_session=/);
  assert.equal((await run(request({ headers: { cookie } }))).allowed, true, "cookie works without Access");

  useAccessMode();
  assert.equal((await run(request({ headers: { cookie } }))).status, 401, "cookie ignored in Access mode");
  const submit = await run(request({ method: "POST", body: { key: API_KEY } }), security.loginSubmitHandler);
  assert.equal(submit.status, 404);
  assert.equal((await run(request({ headers: { accept: "text/html" } }), security.loginPageHandler)).status, 404);
});

test("Access mode: JWKS down with no cached key fails closed with 503", async () => {
  useAccessMode();
  jwks.down = true;
  const out = await run(request({ headers: { "cf-access-jwt-assertion": sign(K1) } }));
  assert.equal(out.allowed, undefined);
  assert.equal(out.status, 503);
});

test("Access mode: exempt routes stay reachable without a token; GitHub webhook still enforces its HMAC", async () => {
  useAccessMode();
  const express = require("express");
  const app = express();
  app.use(require("../dist/webhooks").default);
  app.use(express.json());
  app.use(require("../dist/dashboard").default);
  const server = app.listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  const base = `http://127.0.0.1:${server.address().port}`;
  const tunnel = { "x-forwarded-for": "198.51.100.7" };
  try {
    const linearBody = JSON.stringify({ data: { id: "iss1" } });
    const linear = await realFetch(`${base}/webhooks/linear`, {
      method: "POST",
      headers: { ...tunnel, "content-type": "application/json", "linear-signature": crypto.createHmac("sha256", "linsek").update(linearBody).digest("hex") },
      body: linearBody,
    });
    assert.equal(linear.status, 202, "provider webhook router");

    const ghBody = JSON.stringify({ ref: "refs/heads/feature" });
    const ghSig = "sha256=" + crypto.createHmac("sha256", "ghsek").update(ghBody).digest("hex");
    const ghBad = await realFetch(`${base}/api/webhook/github`, { method: "POST", headers: { ...tunnel, "content-type": "application/json", "x-hub-signature-256": "sha256=" + "0".repeat(64) }, body: ghBody });
    assert.equal(ghBad.status, 403, "GitHub HMAC enforced");
    const ghGood = await realFetch(`${base}/api/webhook/github`, { method: "POST", headers: { ...tunnel, "content-type": "application/json", "x-hub-signature-256": ghSig }, body: ghBody });
    assert.equal(ghGood.status, 200, "GitHub webhook reachable");

    const creds = await realFetch(`${base}/api/discord/creds`, { method: "OPTIONS", headers: tunnel });
    assert.equal(creds.status, 204, "discord creds preflight");

    const cb = await realFetch(`${base}/oauth/gmail/callback`, { headers: tunnel, redirect: "manual" });
    assert.notEqual(cb.status, 401, "OAuth callback");
    assert.ok(!String(cb.headers.get("location") || "").includes("cloudflareaccess"), "OAuth callback not bounced to Access");

    const gated = await realFetch(`${base}/api/ask`, { method: "POST", headers: { ...tunnel, "content-type": "application/json" }, body: "{}" });
    assert.equal(gated.status, 401, "non-exempt route still gated");
  } finally {
    await new Promise((r) => server.close(r));
  }
});
