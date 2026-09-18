const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {
  ModelRouterStore,
  validateConfig,
  publicConfig,
} = require("../dist/modelRouter");
const fixture = () => ({
  version: 1,
  mode: "auto",
  base_url: "http://127.0.0.1:20128/v1",
  api_key: "router-secret", // pii-ok: synthetic test credential
  models: {
    claude: { quality: "cc/claude-opus-4-6", fast: "cc/claude-haiku-4-5" },
    codex: { quality: "cx/gpt-5.4", fast: "cx/gpt-5.4-mini" },
  },
});
test("configuration is atomic, private, and reloaded on each read", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "model-router-"));
  try {
    const store = new ModelRouterStore(path.join(dir, "model-router.json"));
    assert.equal(store.read(), null);
    store.write(fixture());
    assert.equal(fs.statSync(store.file).mode & 0o777, 0o600);
    const second = new ModelRouterStore(store.file);
    const changed = fixture();
    changed.mode = "codex";
    second.write(changed);
    assert.equal(store.read().mode, "codex");
    assert(
      !JSON.stringify(publicConfig(store.read())).includes("router-secret"),
    );
    assert.equal(publicConfig(store.read()).has_key, true);
    assert.throws(() => store.write({ ...changed, mode: "bogus" }));
    assert.equal(store.read().mode, "codex");
    fs.writeFileSync(store.file, "{invalid");
    assert.throws(() => store.read());
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
test("routing cannot send credentials remotely or disguise a different provider", () => {
  for (const base_url of [
    "https://evil.example/v1",
    "http://user:pass@127.0.0.1:20128/v1",
    "http://127.0.0.1:20128/v1?foo=bar",
  ]) {
    assert.throws(() => validateConfig({ ...fixture(), base_url }));
  }
  const value = fixture();
  value.models.claude.quality = "cx/gpt-5.4";
  assert.throws(() => validateConfig(value));
  assert.throws(() => validateConfig({ ...fixture(), api_key: "" }));
});
test("Runpod modes and an exact Tailnet host round trip through the shared config", () => {
  const previous = process.env.AUGMENTAGENT_MODEL_ROUTER_ALLOWED_HOSTS;
  process.env.AUGMENTAGENT_MODEL_ROUTER_ALLOWED_HOSTS =
    "router.fixture.ts.net:20128";
  try {
    const remote = "http://router.fixture.ts.net:20128/v1";
    for (const mode of ["qwen", "glm"]) {
      assert.equal(validateConfig({ ...fixture(), mode, base_url: remote }).mode, mode);
    }
    for (const base_url of [
      "http://other.fixture.ts.net:20128/v1",
      "http://router.fixture.ts.net:20129/v1",
      "http://user:pass@router.fixture.ts.net:20128/v1", // pii-ok: synthetic credentials in a rejection fixture
      "http://router.fixture.ts.net:20128/v1?token=secret",
    ]) {
      assert.throws(() => validateConfig({ ...fixture(), mode: "qwen", base_url }));
    }
  } finally {
    if (previous === undefined) delete process.env.AUGMENTAGENT_MODEL_ROUTER_ALLOWED_HOSTS;
    else process.env.AUGMENTAGENT_MODEL_ROUTER_ALLOWED_HOSTS = previous;
  }
});
const { RouterClient, AccountAuth } = require("../dist/modelRouter");
test("account list strips secrets and supports multiple same-provider accounts", async () => {
  const calls = [];
  const client = new RouterClient(
    { ...fixture(), admin_password: "admin-secret" }, // pii-ok: synthetic test credential
    async (url, init) => {
      calls.push({ url, init });
      if (url.endsWith("/api/auth/login"))
        return new Response("{}", {
          headers: { "Set-Cookie": "auth_token=session; HttpOnly" },
        });
      return Response.json({
        connections: [
          {
            id: "one",
            provider: "codex",
            email: "one@example.com",
            accessToken: "never-show",
            isActive: true,
          },
          {
            id: "two",
            provider: "codex",
            email: "two@example.com",
            refreshToken: "never-show",
            isActive: true,
          },
        ],
      });
    },
  );
  const accounts = await client.accounts();
  assert.equal(accounts.length, 2);
  assert(!JSON.stringify(accounts).includes("never-show"));
  assert.equal(calls[1].init.headers.Cookie, "auth_token=session");
  await assert.rejects(
    () => client.updateAccount("unknown", true, 1),
    /not found/,
  );
});
test("two account logins preserve separate PKCE state and reject mismatches and replay", async () => {
  const exchanges = [];
  let count = 0;
  const auth = new AccountAuth({
    api: async (endpoint, method, body) => {
      if (endpoint.includes("/authorize")) {
        count++;
        return {
          authUrl: "https://auth.openai.com/oauth/authorize",
          state: `state-${count}`,
          codeVerifier: `verifier-${count}`,
        };
      }
      exchanges.push(body);
      return { success: true };
    },
  });
  const first = await auth.start("codex"),
    second = await auth.start("codex");
  assert(!JSON.stringify(first).includes("verifier"));
  await assert.rejects(
    () =>
      auth.finish(
        first.id,
        "http://localhost:1455/auth/callback?code=x&state=state-2",
      ),
    /state/,
  );
  assert.equal(exchanges.length, 0);
  await auth.finish(
    second.id,
    "http://localhost:1455/auth/callback?code=two&state=state-2",
  );
  await auth.finish(
    first.id,
    "http://localhost:1455/auth/callback?code=one&state=state-1",
  );
  assert.equal(exchanges[0].codeVerifier, "verifier-2");
  assert.equal(exchanges[1].codeVerifier, "verifier-1");
  await assert.rejects(
    () =>
      auth.finish(
        first.id,
        "http://localhost:1455/auth/callback?code=one&state=state-1",
      ),
    /expired/,
  );
});

test("filesystem write errors are sanitized without changing the old file", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "router-write-"));
  try {
    const parent = path.join(dir, "private-path");
    fs.writeFileSync(parent, "unchanged");
    const store = new ModelRouterStore(path.join(parent, "router.json"));
    assert.throws(
      () => store.write(fixture()),
      (e) => e.message === "Cannot save model router configuration",
    );
    assert.equal(fs.readFileSync(parent, "utf8"), "unchanged");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
test("new OAuth sessions use current credentials while pending sessions retain their gateway", async () => {
  let selected = "first";
  const exchanges = [];
  const auth = new AccountAuth(() => {
    const gateway = selected;
    return {
      api: async (endpoint, method, body) => {
        return endpoint.includes("/authorize")
          ? {
              authUrl: "https://auth.openai.com/oauth/authorize",
              state: gateway,
              codeVerifier: gateway,
            }
          : exchanges.push({ gateway, body });
      },
    };
  });
  // Each resolver result captures the selected gateway, rather than retaining the first client.
  const first = await auth.start("codex");
  selected = "second";
  const second = await auth.start("codex");
  await auth.finish(
    second.id,
    "http://localhost:1455/auth/callback?code=x&state=second",
  );
  await auth.finish(
    first.id,
    "http://localhost:1455/auth/callback?code=x&state=first",
  );
  assert.deepEqual(
    exchanges.map((e) => e.gateway),
    ["second", "first"],
  );
});
