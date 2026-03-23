import { Router } from "express";
import {
  getActions,
  getActionById,
  getStats,
  getSenders,
  addSender,
  removeSender,
  toggleSender,
  getConfig,
  setConfig,
  deleteConfig,
  getActionCount,
} from "./db";
import type { ActionStatus } from "./types";

const router = Router();

// --- Page Routes ---

router.get("/", (_req, res) => {
  res.redirect("/dashboard");
});

router.get("/dashboard", (_req, res) => {
  const stats = getStats();
  const recentActions = getActions({ limit: 10 });
  res.render("dashboard", { stats, actions: recentActions, page: "dashboard" });
});

router.get("/history", (req, res) => {
  const status = req.query.status as ActionStatus | undefined;
  const page = parseInt(req.query.page as string) || 1;
  const limit = 20;
  const offset = (page - 1) * limit;

  const actions = getActions({ limit, offset, status });
  const total = getActionCount(status);
  const totalPages = Math.ceil(total / limit);

  res.render("history", {
    actions,
    currentStatus: status || "all",
    currentPage: page,
    totalPages,
    total,
    page: "history",
  });
});

router.get("/settings", (_req, res) => {
  const senders = getSenders();
  const configStatus = {
    groqKey: !!getConfig("groq_api_key"),
    composioKey: !!getConfig("composio_api_key"),
    discordWebhook: !!getConfig("discord_webhook_url"),
    discordBotToken: !!getConfig("discord_bot_token"),
  };
  res.render("settings", { senders, configStatus, page: "settings" });
});

// --- HTMX API Routes ---

router.get("/api/stats", (_req, res) => {
  const stats = getStats();
  res.render("partials/stats", { stats });
});

router.get("/api/actions", (req, res) => {
  const status = req.query.status as ActionStatus | undefined;
  const page = parseInt(req.query.page as string) || 1;
  const limit = 20;
  const offset = (page - 1) * limit;

  const actions = getActions({
    limit,
    offset,
    status: status === ("all" as any) ? undefined : status,
  });
  const total = getActionCount(status === ("all" as any) ? undefined : status);
  const totalPages = Math.ceil(total / limit);

  res.render("partials/action-rows", {
    actions,
    currentPage: page,
    totalPages,
    currentStatus: status || "all",
  });
});

router.get("/api/actions/:id/preview", (req, res) => {
  const action = getActionById(req.params.id);
  if (!action) {
    res.status(404).send("<p>Action not found</p>");
    return;
  }
  res.render("partials/draft-preview", { action });
});

// --- Senders API ---

router.post("/api/senders", (req, res) => {
  const { email, label } = req.body;
  if (!email || !email.includes("@")) {
    res.status(400).send('<p class="text-red-400">Invalid email address</p>');
    return;
  }
  addSender(email, label);
  const senders = getSenders();
  res.render("partials/sender-list", { senders });
});

router.delete("/api/senders/:id", (req, res) => {
  removeSender(req.params.id);
  res.send("");
});

router.patch("/api/senders/:id/toggle", (req, res) => {
  toggleSender(req.params.id);
  const senders = getSenders();
  res.render("partials/sender-list", { senders });
});

// --- Config API ---

router.post("/api/config", (req, res) => {
  const { key, value } = req.body;
  const allowedKeys = [
    "groq_api_key",
    "composio_api_key",
    "discord_webhook_url",
    "discord_bot_token",
  ];

  if (!allowedKeys.includes(key)) {
    res.status(400).send('<p class="text-red-400">Invalid config key</p>');
    return;
  }

  if (value) {
    setConfig(key, value);
  } else {
    deleteConfig(key);
  }

  const configStatus = {
    groqKey: !!getConfig("groq_api_key"),
    composioKey: !!getConfig("composio_api_key"),
    discordWebhook: !!getConfig("discord_webhook_url"),
    discordBotToken: !!getConfig("discord_bot_token"),
  };

  res.render("partials/config-status", { configStatus });
});

router.get("/api/config/status", (_req, res) => {
  const configStatus = {
    groqKey: !!getConfig("groq_api_key"),
    composioKey: !!getConfig("composio_api_key"),
    discordWebhook: !!getConfig("discord_webhook_url"),
    discordBotToken: !!getConfig("discord_bot_token"),
  };
  res.render("partials/config-status", { configStatus });
});

export default router;
