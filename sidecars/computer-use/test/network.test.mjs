import { test } from "node:test";
import assert from "node:assert/strict";
import { pinnedAddresses, forward } from "../network.mjs";
test("all DNS answers must be public and the selected address is pinned for connection", async () => {
  assert.deepEqual(
    await pinnedAddresses("fixture.test", async () => [
      { address: "93.184.216.34", family: 4 },
    ]),
    [{ address: "93.184.216.34", family: 4 }],
  );
  await assert.rejects(
    pinnedAddresses("fixture.test", async () => [
      { address: "93.184.216.34", family: 4 },
      { address: "127.0.0.1", family: 4 },
    ]),
    /address_blocked/,
  );
  await assert.rejects(
    pinnedAddresses("fixture.test", async () => [
      { address: "::ffff:127.0.0.1", family: 6 },
    ]),
    /address_blocked/,
  );
});
test("real TLS connection uses the pinned address and rejects a rebinding answer", async (t) => {
  const https = await import("node:https");
  const fs = await import("node:fs/promises");
  const { execFileSync } = await import("node:child_process");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const dir = await fs.mkdtemp(join(tmpdir(), "egress-fixture-"));
  const key = join(dir, "key.pem"),
    cert = join(dir, "cert.pem");
  execFileSync(
    "openssl",
    [
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      key,
      "-out",
      cert,
      "-days",
      "1",
      "-subj",
      "/CN=fixture.test",
      "-addext",
      "subjectAltName=DNS:fixture.test",
    ],
    { stdio: "ignore" },
  );
  const certificate = await fs.readFile(cert);
  const tls = { key: await fs.readFile(key), cert: certificate };
  let goodHits = 0,
    badHits = 0,
    calls = 0;
  const good = https.createServer(tls, (_req, res) => {
    goodHits++;
    res.setHeader("content-type", "text/html");
    res.end("<body>fixture</body>");
  });
  await new Promise((r) => good.listen(0, "127.0.0.1", r));
  const port = good.address().port;
  const bad = https.createServer(tls, (_req, res) => {
    badHits++;
    res.end("PRIVATE");
  });
  await new Promise((r) => bad.listen(port, "127.0.0.2", r));
  t.after(async () => {
    await Promise.all([
      new Promise((r) => good.close(r)),
      new Promise((r) => bad.close(r)),
    ]);
    await fs.rm(dir, { recursive: true, force: true });
  });
  const origin = `https://fixture.test:${port}`;
  const frame = { page: () => ({ mainFrame: () => frame }) };
  const req = {
    url: () => origin + "/",
    method: () => "GET",
    isNavigationRequest: () => true,
    frame: () => frame,
    allHeaders: async () => ({}),
    postDataBuffer: () => null,
  };
  const options = {
    fixtureOrigin: origin,
    ca: certificate,
    lookup: async () => [
      { address: ++calls === 1 ? "127.0.0.1" : "127.0.0.2", family: 4 },
    ],
  };
  const response = await forward(req, ["fixture.test"], options);
  assert.equal(response.status, 200);
  assert.match(
    response.headers["content-security-policy"],
    /worker-src 'none'/,
  );
  assert.match(
    response.headers["content-security-policy"],
    /sandbox allow-scripts allow-same-origin allow-forms/,
  );
  assert.equal(goodHits, 1);
  assert.equal(calls, 1);
  await assert.rejects(
    forward(req, ["fixture.test"], options),
    /address_blocked/,
  );
  assert.equal(badHits, 0);
});
test("reserved IPv6 destinations never qualify as public egress", async () => {
  for (const address of [
    "fec0::1",
    "100::1",
    "2001::1",
    "2001:2::1",
    "2002:7f00:1::1",
    "3fff::1",
  ])
    await assert.rejects(
      pinnedAddresses("fixture.test", async () => [{ address, family: 6 }]),
      /address_blocked/,
    );
  assert.equal(
    (
      await pinnedAddresses("fixture.test", async () => [
        { address: "2606:4700:4700::1111", family: 6 },
      ])
    ).length,
    1,
  );
});
