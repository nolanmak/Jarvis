import { Router } from "express";
import {
  ModelRouterStore,
  RouterClient,
  AccountAuth,
  publicConfig,
  validateConfig,
  RouterConfig,
  Provider,
} from "./modelRouter";

// Mounted behind the dashboard's existing authentication and Host/Origin guards.
export function createModelRouterRoutes(
  store = new ModelRouterStore(),
  clientFor = (c: RouterConfig) => new RouterClient(c),
) {
  const router = Router();
  // Serialize settings and account writes so concurrent disables cannot both
  // observe the other account as enabled and leave the route unusable.
  let mutationTail: Promise<void> = Promise.resolve();
  function mutate<T>(operation: () => Promise<T>): Promise<T> {
    const result = mutationTail.then(operation);
    mutationTail = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }
  const configured = () => {
    const config = store.read();
    if (!config)
      throw new Error(
        "9Router is not installed. Run scripts/install-model-router.py on the agent host.",
      );
    return config;
  };
  const auth = new AccountAuth(() => clientFor(configured()));
  router.use("/model-accounts", (_req, res, next) => {
    res.set("Cache-Control", "no-store");
    next();
  });
  router.get("/model-accounts", async (_req, res) => {
    let config = null,
      accounts: Awaited<ReturnType<RouterClient["accounts"]>> = [],
      error = null;
    try {
      const c = store.read();
      config = publicConfig(c);
      if (c) accounts = await clientFor(c).accounts();
    } catch (e) {
      error = e instanceof Error ? e.message : "9Router is unavailable";
    }
    res.render("model-accounts", { config, accounts, error, page: "settings" });
  });
  router.post("/model-accounts/settings", async (req, res) => {
    try {
      await mutate(async () => {
        const previous = configured();
        const next = validateConfig({
          ...previous,
          mode: req.body.mode,
          models: {
            claude: {
              quality: req.body.claude_quality,
              fast: req.body.claude_fast,
            },
            codex: {
              quality: req.body.codex_quality,
              fast: req.body.codex_fast,
            },
          },
        });
        if (next.mode === "qwen" || next.mode === "glm") {
          const flag = next.mode === "qwen"
            ? "AUGMENTAGENT_MODEL_QWEN_ENABLED"
            : "AUGMENTAGENT_MODEL_GLM_ENABLED";
          if (process.env[flag] !== "1")
            throw new Error(`${next.mode.toUpperCase()} is paused until Runpod verification`);
        } else if (next.mode !== "direct") {
          const accounts = await clientFor(previous).accounts();
          const providers =
            next.mode === "auto" ? ["claude", "codex"] : [next.mode];
          for (const provider of providers)
            if (!accounts.some((a) => a.provider === provider && a.active))
              throw new Error(
                `Connect and enable a ${provider} account before selecting this route`,
              );
        }
        store.write(next);
      });
      res.redirect(303, "/model-accounts");
    } catch (e) {
      res.status(400).render("model-account-error", {
        error: e instanceof Error ? e.message : "Unable to save routing",
        page: "settings",
      });
    }
  });
  router.post("/model-accounts/accounts/:id", async (req, res) => {
    try {
      await mutate(async () => {
        const config = configured();
        const client = clientFor(config);
        const id = String(req.params.id);
        const active = req.body.active === "on";
        if (!active && config.mode !== "direct") {
          const accounts = await client.accounts();
          const target = accounts.find((a) => a.id === id);
          if (
            target &&
            (config.mode === "auto" || config.mode === target.provider) &&
            !accounts.some(
              (a) => a.id !== id && a.provider === target.provider && a.active,
            )
          ) {
            throw new Error(
              "Enable another account or switch routing before disabling the last required account",
            );
          }
        }
        await client.updateAccount(id, active, Number(req.body.priority));
      });
      res.redirect(303, "/model-accounts");
    } catch (e) {
      res.status(400).render("model-account-error", {
        error: e instanceof Error ? e.message : "Unable to update account",
        page: "settings",
      });
    }
  });
  router.post("/model-accounts/connect", async (req, res) => {
    try {
      const connection = await auth.start(req.body.provider as Provider);
      res.render("model-account-connect", { connection, page: "settings" });
    } catch (e) {
      res.status(400).render("model-account-error", {
        error: e instanceof Error ? e.message : "Unable to start connection",
        page: "settings",
      });
    }
  });
  router.post("/model-accounts/complete", async (req, res) => {
    try {
      await auth.finish(
        String(req.body.id || ""),
        String(req.body.callback || "").trim(),
      );
      res.redirect(303, "/model-accounts");
    } catch (e) {
      res.status(400).render("model-account-error", {
        error: e instanceof Error ? e.message : "Unable to connect account",
        page: "settings",
      });
    }
  });
  return router;
}
