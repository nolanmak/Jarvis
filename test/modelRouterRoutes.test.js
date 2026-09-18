const test = require("node:test");
const assert = require("node:assert/strict");
const express = require("express");
const path = require("node:path");
const { createModelRouterRoutes } = require("../dist/modelRouterRoutes");
const config = {
  version: 1,
  mode: "direct",
  base_url: "http://127.0.0.1:20128/v1",
  api_key: "hidden-key",
  admin_password: "hidden-password", // pii-ok: synthetic test credential
  models: {
    claude: { quality: "cc/claude-opus-4-6", fast: "cc/claude-haiku-4-5" },
    codex: { quality: "cx/gpt-5.4", fast: "cx/gpt-5.4-mini" },
  },
};
test("settings render without secrets, validate accounts, and switch routes", async () => {
  let saved = structuredClone(config);
  const store = {
    read: () => saved,
    write: (c) => {
      saved = c;
    },
  };
  const client = {
    accounts: async () => [
      {
        id: "account-a",
        provider: "codex",
        name: "Account A",
        active: true,
        priority: 1,
        status: "active",
      },
    ],
  };
  const app = express();
  app.use(express.urlencoded({ extended: false }));
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "../views"));
  app.use(createModelRouterRoutes(store, () => client));
  const server = app.listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    const html = await (await fetch(base + "/model-accounts")).text();
    assert(html.includes("Account A"));
    assert(html.includes("Connect Codex"));
    assert(!html.includes("hidden-key"));
    assert(!html.includes("hidden-password"));
    const form = new URLSearchParams({
      mode: "codex",
      claude_quality: config.models.claude.quality,
      claude_fast: config.models.claude.fast,
      codex_quality: config.models.codex.quality,
      codex_fast: config.models.codex.fast,
    });
    let response = await fetch(base + "/model-accounts/settings", {
      method: "POST",
      body: form,
      redirect: "manual",
    });
    assert.equal(response.status, 303);
    assert.equal(saved.mode, "codex");
    form.set("mode", "claude");
    response = await fetch(base + "/model-accounts/settings", {
      method: "POST",
      body: form,
      redirect: "manual",
    });
    assert.equal(response.status, 400);
    assert.equal(saved.mode, "codex");
  } finally {
    await new Promise((r) => server.close(r));
  }
});

test("account mutations require dashboard authentication and reject foreign origins", async () => {
  process.env.AUGMENTAGENT_API_KEY = "synthetic-dashboard-key";
  const { requireAuth, hostOriginGuard } = require("../dist/security");
  const app = express();
  app.use(hostOriginGuard);
  app.use(express.urlencoded({ extended: false }));
  app.use(requireAuth);
  let calls = 0;
  app.use(
    createModelRouterRoutes(
      {
        read: () => {
          calls++;
          return structuredClone(config);
        },
        write: () => {},
      },
      () => ({ accounts: async () => [] }),
    ),
  );
  const server = app.listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  process.env.DASHBOARD_PORT = String(server.address().port);
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    let response = await fetch(base + "/model-accounts/connect", {
      method: "POST",
      headers: { Accept: "application/json" },
      body: new URLSearchParams({ provider: "codex" }),
      redirect: "manual",
    });
    assert.equal(response.status, 401);
    assert.equal(calls, 0);
    response = await fetch(base + "/model-accounts/settings", {
      method: "POST",
      headers: {
        Authorization: "Bearer synthetic-dashboard-key",
        Origin: "https://foreign.example",
      },
      body: new URLSearchParams({ mode: "direct" }),
      redirect: "manual",
    });
    assert.equal(response.status, 403);
    assert.equal(calls, 0);
  } finally {
    await new Promise((r) => server.close(r));
  }
});
