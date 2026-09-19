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
    const previousQwenEnabled = process.env.AUGMENTAGENT_MODEL_QWEN_ENABLED;
    delete process.env.AUGMENTAGENT_MODEL_QWEN_ENABLED;
    form.set("mode", "qwen");
    try {
      response = await fetch(base + "/model-accounts/settings", {
        method: "POST", body: form, redirect: "manual",
      });
      assert.equal(response.status, 400, "a paused Runpod route must not be activated");
      process.env.AUGMENTAGENT_MODEL_QWEN_ENABLED = "1";
      response = await fetch(base + "/model-accounts/settings", {
        method: "POST", body: form, redirect: "manual",
      });
      assert.equal(response.status, 303);
      assert.equal(saved.mode, "qwen");
    } finally {
      if (previousQwenEnabled === undefined)
        delete process.env.AUGMENTAGENT_MODEL_QWEN_ENABLED;
      else
        process.env.AUGMENTAGENT_MODEL_QWEN_ENABLED = previousQwenEnabled;
    }
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

test("concurrent account disables cannot remove the last account required by the current route", async () => {
  let saved = { ...structuredClone(config), mode: "codex" };
  const accounts = [
    { id: "a", provider: "codex", active: true },
    { id: "b", provider: "codex", active: true },
  ];
  const client = {
    accounts: async () => {
      await new Promise((r) => setTimeout(r, 5));
      return structuredClone(accounts);
    },
    updateAccount: async (id, active) => {
      await new Promise((r) => setTimeout(r, 5));
      accounts.find((a) => a.id === id).active = active;
    },
  };
  const app = express();
  app.use(express.urlencoded({ extended: false }));
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "../views"));
  app.use(
    createModelRouterRoutes(
      {
        read: () => saved,
        write: (c) => {
          saved = c;
        },
      },
      () => client,
    ),
  );
  const server = app.listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    const results = await Promise.all(
      ["a", "b"].map((id) =>
        fetch(base + "/model-accounts/accounts/" + id, {
          method: "POST",
          body: new URLSearchParams({ priority: "1" }),
          redirect: "manual",
        }),
      ),
    );
    assert.deepEqual(results.map((r) => r.status).sort(), [303, 400]);
    assert.equal(accounts.filter((a) => a.active).length, 1);
    saved.mode = "direct";
    const remaining = accounts.find((a) => a.active).id;
    const response = await fetch(
      base + "/model-accounts/accounts/" + remaining,
      {
        method: "POST",
        body: new URLSearchParams({ priority: "1" }),
        redirect: "manual",
      },
    );
    assert.equal(
      response.status,
      303,
      "direct mode can disable all router accounts",
    );
  } finally {
    await new Promise((r) => server.close(r));
  }
});
