import { Router, Request } from "express";
import { exec, spawn } from "child_process";
import crypto from "crypto";
import fs from "fs";
import os from "os";
import path from "path";
import multer from "multer";
import Composio from "@composio/client";
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
  getGmailAccounts,
  getActiveGmailAccounts,
  addGmailAccount,
  removeGmailAccount,
  getDriveAccounts,
  getActiveDriveAccounts,
  addDriveAccount,
  removeDriveAccount,
  getSocialApiAccounts,
  upsertSocialApiAccount,
  setSocialApiAccountActive,
  removeSocialApiAccount,
  hasAnyGmailAccount,
  getEmailCount,
  purgeOldEmails,
  listSubscriptions,
  getSubscription,
  upsertSubscription,
  updateSubscriptionMode,
  deleteSubscription,
  getActiveSlackWorkspaces,
  getSlackWorkspaces,
  getSlackWorkspaceByTeam,
  deactivateSlackWorkspace,
  listProactiveSignals,
  listProactiveSignalsForPerson,
  getProactiveSignal,
  dismissProactiveSignal,
  snoozeProactiveSignal,
  recordProactiveUserAction,
  clearProactiveUserAction,
  listActiveProactiveUserActions,
} from "./db";
import {
  listAgentRepos,
  upsertAgentRepo,
  revokeAgentRepo,
  listAgentPrRuns,
  resolveAgentPrRun,
} from "./db";
import type { ActionStatus, SubscriptionMode } from "./types";
import { runAgentQuery } from "./agent";
import { listDms, listGuilds, listGuildChannels, discordStatus } from "./discordApi";
import {
  listConversations as listSlackConversations,
  persistSlackAuth,
  runCli,
} from "./slackApi";
import { requireApiKey } from "./apiV1";

const router = Router();

// #479: derive a browsable https URL for the private knowledge-base repo from
// AUGMENTAGENT_WIKI_REMOTE (a git remote URL or an `owner/repo` slug). Returns
// "" when unset, so the dashboard simply hides the link rather than pointing at
// a hardcoded operator repo.
function knowledgeBaseUrl(): string {
  const raw = (process.env.AUGMENTAGENT_WIKI_REMOTE || "").trim();
  if (!raw) return "";
  let u = raw.replace(/\.git$/, "");
  const ssh = u.match(/^git@([^:]+):(.+)$/); // scp-style ssh remote -> https
  if (ssh) u = `https://${ssh[1]}/${ssh[2]}`;
  else if (/^[\w.-]+\/[\w.-]+$/.test(u)) u = `https://github.com/${u}`; // bare slug
  return u;
}

// Expose it to every dashboard view (the header partial reads `kbUrl`).
router.use((_req, res, next) => {
  res.locals.kbUrl = knowledgeBaseUrl();
  next();
});

function getComposioClient(): Composio | null {
  const apiKey = getConfig("composio_api_key") || process.env.COMPOSIO_API_KEY;
  if (!apiKey) return null;
  return new Composio({ apiKey });
}

// #179 — How long since the last successful gmail poll counts as "still
// connected." The daemon polls every ~2 min in steady state, so 5 min
// covers a missed cycle (transient network blip) but flips the indicator
// off after two consecutive failures — long enough that one bad poll
// doesn't toggle the UI, short enough that a revoked OAuth grant or a
// switched Composio project shows up within minutes rather than the
// indefinite-stale state #179 describes.
const GMAIL_LIVENESS_STALE_MS = 5 * 60 * 1000;

// #398/#400 — per-entity Google Calendar connection status, cached 60s so
// config-status renders (every htmx swap hits it) don't each round-trip to
// Composio. The callback route nulls the cache so a fresh connect flips the
// badge immediately.
let calendarStatusCache: { at: number; entities: string[] } | null = null;

async function getCalendarConnectedEntities(): Promise<string[]> {
  const client = getComposioClient();
  if (!client) return [];
  if (calendarStatusCache && Date.now() - calendarStatusCache.at < 60_000) {
    return calendarStatusCache.entities;
  }
  try {
    const conns = await client.connectedAccounts.list({
      toolkit_slugs: ["googlecalendar"],
    });
    const entities = conns.items
      .filter((c) => c.status === "ACTIVE")
      .map((c) => (c as any).user_id || (c as any).entity_id)
      .filter(Boolean);
    calendarStatusCache = { at: Date.now(), entities };
    return entities;
  } catch (err) {
    console.error(
      "[calendar] connection status check failed:",
      err instanceof Error ? err.message : err,
    );
    // Stale beats missing: keep showing the last known state on a blip.
    return calendarStatusCache?.entities ?? [];
  }
}

// Check both DB config and env vars for integration status
async function getConfigStatus() {
  const gmailAccounts = getGmailAccounts();
  // #179 — "connected" means the most recent poll *succeeded* recently.
  // `active=1` alone doesn't prove the connection still works at Composio
  // (key rotations / project switches / revoked grants leave `active=1`
  // forever). `lastPolledAt=null` means the daemon hasn't polled yet —
  // treat as not-yet-verified to surface real state once the first poll
  // lands (within ~2 min of a fresh connect).
  const nowMs = Date.now();
  const gmailConnected = gmailAccounts.some(
    (a) =>
      a.active &&
      a.lastPollOk === 1 &&
      a.lastPolledAt != null &&
      nowMs - a.lastPolledAt < GMAIL_LIVENESS_STALE_MS,
  );
  return {
    groqKey: !!(getConfig("groq_api_key") || process.env.GROQ_API_KEY),
    cerebrasKey: !!(getConfig("cerebras_api_key") || process.env.CEREBRAS_API_KEY),
    composioKey: !!(getConfig("composio_api_key") || process.env.COMPOSIO_API_KEY),
    gmailAccounts,
    gmailConnected,
    // Entity ids with an ACTIVE googlecalendar connection (#398/#400) —
    // the template matches these against gmailAccounts[].entityId.
    calendarEntities: await getCalendarConnectedEntities(),
    discordWebhook: !!(getConfig("discord_webhook_url") || process.env.DISCORD_WEBHOOK_URL),
    discordBotToken: !!(getConfig("discord_bot_token") || process.env.DISCORD_BOT_TOKEN),
    emailRetentionDays: getConfig("email_retention_days") || "0",
    emailCount: getEmailCount(),
  };
}

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
  const platform = req.query.platform as string | undefined;
  const page = parseInt(req.query.page as string) || 1;
  const limit = 20;
  const offset = (page - 1) * limit;

  const actions = getActions({ limit, offset, status, platform });
  const total = getActionCount(status, platform);
  const totalPages = Math.ceil(total / limit);

  res.render("history", {
    actions,
    currentStatus: status || "all",
    currentPlatform: platform || "all",
    currentPage: page,
    totalPages,
    total,
    page: "history",
  });
});

// Mask a secret for display: show the first 4 and last 4 chars, dots between.
// Short keys are fully masked. Returns null when no key is set.
function maskSecret(raw: string | null): string | null {
  if (!raw) return null;
  if (raw.length <= 8) return "•".repeat(raw.length);
  return `${raw.slice(0, 4)}${"•".repeat(8)}${raw.slice(-4)}`;
}

router.get("/settings", async (_req, res) => {
  const senders = getSenders();
  const configStatus = await getConfigStatus();
  const emailRetention = getConfig("email_retention_days") || "0";
  const emailCount = getEmailCount();
  res.render("settings", {
    senders,
    configStatus,
    emailRetention,
    emailCount,
    socialApiKeyMasked: maskSecret(getConfig("socialapi_api_key")),
    socialApiAccounts: getSocialApiAccounts(),
    socialApiError: null,
    page: "settings",
  });
});

router.get("/subscriptions", (_req, res) => {
  const subs = listSubscriptions();
  res.render("subscriptions", { subs, page: "subscriptions" });
});

// --- #117 Multi-repo agent-coding access controls ---
//
// `/repos` is the admin view for the allowlist (#117). The page shell
// itself is just a UI scaffold; every mutating + data route below is gated
// by `requireApiKey` (the SAME middleware the v1 machine API uses). The
// dashboard process doesn't mount the v1 router, so the guard is applied
// per-route here. Default-deny: an empty `agent_repos` table = the agent
// can touch nothing.

function fullNameValid(s: unknown): s is string {
  return typeof s === "string" && /^[\w.-]+\/[\w.-]+$/.test(s.trim());
}

router.get("/repos", (_req, res) => {
  res.render("repos", { page: "repos" });
});

// JSON: allowlist + recent run history (key-gated).
router.get("/api/repos", requireApiKey, (_req, res) => {
  const repos = listAgentRepos(false).map((r) => ({
    ...r,
    enabled: !!r.enabled,
  }));
  res.json(repos);
});

router.get("/api/repos/runs", requireApiKey, (req, res) => {
  const repo = (req.query.full_name as string) || undefined;
  const limit = Math.min(parseInt(req.query.limit as string) || 50, 200);
  res.json(listAgentPrRuns(repo, limit));
});

// Allowlist (or re-grant / update) a repo.
router.post("/api/repos", requireApiKey, (req, res) => {
  const {
    full_name,
    base_branch,
    build_cmd,
    blast_radius_extra,
    max_diff_lines,
  } = req.body || {};
  if (!fullNameValid(full_name)) {
    res.status(400).json({ error: "full_name must be 'owner/name'" });
    return;
  }
  const cap = parseInt(max_diff_lines) || 600;
  const row = upsertAgentRepo(
    String(full_name).trim(),
    (base_branch && String(base_branch).trim()) || "main",
    build_cmd ? String(build_cmd) : "",
    blast_radius_extra ? String(blast_radius_extra) : "",
    cap > 0 ? cap : 600
  );
  res.status(201).json({ ...row, enabled: !!row.enabled });
});

// Revoke a repo (soft-disable + auto-reject in-flight gate rows). The
// `owner/name` slug carries a slash, so it's sent as two path segments.
router.delete("/api/repos/:owner/:name", requireApiKey, (req, res) => {
  const fullName = `${String(req.params.owner)}/${String(req.params.name)}`;
  if (!fullNameValid(fullName)) {
    res.status(400).json({ error: "invalid full_name" });
    return;
  }
  const cancelled = revokeAgentRepo(fullName);
  res.json({ revoked: fullName, cancelled_runs: cancelled });
});

// Approve / reject a queued gate row. The Rust loop's `--approve-open`
// pass opens the draft PR for `approved` rows; nothing is auto-merged.
router.post("/api/repos/runs/:id/:decision", requireApiKey, (req, res) => {
  const id = String(req.params.id);
  const decision = String(req.params.decision);
  if (decision !== "approved" && decision !== "rejected") {
    res.status(400).json({ error: "decision must be approved|rejected" });
    return;
  }
  const row = resolveAgentPrRun(id, decision);
  if (!row) {
    res
      .status(409)
      .json({ error: "run not pending (already resolved or unknown)" });
    return;
  }
  res.json(row);
});

// --- Subscription CRUD ---

const ALLOWED_MODES: SubscriptionMode[] = ["priority", "digest", "store_only"];
const ALLOWED_PLATFORMS = new Set(["discord", "slack"]);

router.get("/api/subscriptions", (_req, res) => {
  const subs = listSubscriptions();
  res.render("partials/subscription-rows", { subs });
});

router.post("/api/subscriptions", (req, res) => {
  const { platform, channel_id, display_name, mode, account_id } = req.body as {
    platform?: string;
    channel_id?: string;
    display_name?: string;
    mode?: string;
    account_id?: string;
  };
  if (!platform || !ALLOWED_PLATFORMS.has(platform)) {
    return res.status(400).send("invalid platform");
  }
  if (!channel_id || !display_name) {
    return res.status(400).send("channel_id and display_name required");
  }
  if (!mode || !ALLOWED_MODES.includes(mode as SubscriptionMode)) {
    return res.status(400).send("invalid mode");
  }
  // Slack requires an account_id (team_id) so cross-workspace channel collisions
  // don't alias. Default to the sole connected workspace when the UI omits it.
  let resolvedAccountId: string | null = account_id ?? null;
  if (platform === "slack") {
    if (!resolvedAccountId) {
      const workspaces = getActiveSlackWorkspaces();
      if (workspaces.length === 1) {
        resolvedAccountId = workspaces[0].teamId;
      } else if (workspaces.length === 0) {
        return res
          .status(400)
          .send("no slack workspace connected — connect one in Subscriptions first");
      } else {
        return res.status(400).send("account_id (team_id) required for slack");
      }
    }
  } else {
    resolvedAccountId = null;
  }
  upsertSubscription(
    platform,
    channel_id,
    display_name,
    mode as SubscriptionMode,
    resolvedAccountId
  );
  const subs = listSubscriptions();
  return res.render("partials/subscription-rows", { subs });
});

router.put("/api/subscriptions/:id", (req, res) => {
  const id = req.params.id;
  const mode = req.body?.mode as string | undefined;
  if (!mode || !ALLOWED_MODES.includes(mode as SubscriptionMode)) {
    return res.status(400).send("invalid mode");
  }
  if (!getSubscription(id)) return res.status(404).send("not found");
  updateSubscriptionMode(id, mode as SubscriptionMode);
  const subs = listSubscriptions();
  return res.render("partials/subscription-rows", { subs });
});

router.delete("/api/subscriptions/:id", (req, res) => {
  const id = req.params.id;
  if (!getSubscription(id)) return res.status(404).send("not found");
  deleteSubscription(id);
  const subs = listSubscriptions();
  return res.render("partials/subscription-rows", { subs });
});

// --- Discord source-picker proxies (shell out to Rust CLI) ---

router.get("/api/discord/status", async (_req, res) => {
  try {
    const s = await discordStatus();
    res.json(s);
  } catch (e) {
    res.json({ connected: false, error: (e as Error).message });
  }
});

// --- Discord credential ingest (bookmarklet target) ---
//
// The bookmarklet runs on https://discord.com, hooks fetch/XHR to capture the
// `authorization` and `x-super-properties` headers from any outgoing request
// the Discord client makes (heartbeats, presence updates, channel switches),
// then POSTs the four credential fields here. We CORS-allow only discord.com.
const DISCORD_CREDS_ALLOW_ORIGIN = "https://discord.com";

router.options("/api/discord/creds", (_req, res) => {
  res.header("Access-Control-Allow-Origin", DISCORD_CREDS_ALLOW_ORIGIN);
  res.header("Access-Control-Allow-Methods", "POST, OPTIONS");
  res.header("Access-Control-Allow-Headers", "Content-Type");
  res.header("Access-Control-Max-Age", "600");
  res.sendStatus(204);
});

router.post("/api/discord/creds", (req, res) => {
  res.header("Access-Control-Allow-Origin", DISCORD_CREDS_ALLOW_ORIGIN);
  const { user_id, token, super_properties_b64, user_agent } = (req.body || {}) as {
    user_id?: unknown;
    token?: unknown;
    super_properties_b64?: unknown;
    user_agent?: unknown;
  };
  const missing: string[] = [];
  if (typeof user_id !== "string" || !/^\d{15,25}$/.test(user_id)) missing.push("user_id");
  if (typeof token !== "string" || token.length < 20) missing.push("token");
  if (typeof super_properties_b64 !== "string" || super_properties_b64.length < 20)
    missing.push("super_properties_b64");
  if (typeof user_agent !== "string" || user_agent.length < 10) missing.push("user_agent");
  if (missing.length) return res.status(400).json({ error: `bad/missing fields: ${missing.join(", ")}` });

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "aa-discord-"));
  const file = path.join(dir, "creds.json");
  fs.writeFileSync(
    file,
    JSON.stringify({ user_id, token, super_properties_b64, user_agent }),
    { mode: 0o600 },
  );
  const cli = fs.existsSync(path.resolve(process.cwd(), "target/release/augmentagent"))
    ? path.resolve(process.cwd(), "target/release/augmentagent")
    : path.resolve(process.cwd(), "target/debug/augmentagent");

  const child = spawn(cli, ["discord", "login", "--creds-json", file], { stdio: ["ignore", "pipe", "pipe"] });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (d) => (stdout += d.toString()));
  child.stderr.on("data", (d) => (stderr += d.toString()));
  const cleanup = () => {
    try { fs.unlinkSync(file); } catch (_) { /* ignore */ }
    try { fs.rmdirSync(dir); } catch (_) { /* ignore */ }
  };
  child.on("error", (err) => {
    cleanup();
    res.status(500).json({ error: `spawn failed: ${err.message}` });
  });
  child.on("exit", (code) => {
    cleanup();
    if (code !== 0) {
      return res.status(500).json({ error: stderr.trim() || stdout.trim() || `discord login exited ${code}` });
    }
    res.json({ ok: true, user_id });
  });
});

router.get("/api/discord/dms", async (_req, res) => {
  try {
    const dms = await listDms();
    res.json(dms);
  } catch (e) {
    res.status(500).json({ error: (e as Error).message });
  }
});

router.get("/api/discord/guilds", async (_req, res) => {
  try {
    const guilds = await listGuilds();
    res.json(guilds);
  } catch (e) {
    res.status(500).json({ error: (e as Error).message });
  }
});

router.get("/api/discord/guilds/:id/channels", async (req, res) => {
  try {
    const channels = await listGuildChannels(req.params.id);
    res.json(channels);
  } catch (e) {
    res.status(500).json({ error: (e as Error).message });
  }
});

// --- Slack source-picker proxy (shells to Rust CLI) ---

router.get("/api/slack/conversations", async (req, res) => {
  const types = (req.query.types as string | undefined) ||
    "public_channel,private_channel,im,mpim";
  const teamId = req.query.team_id as string | undefined;
  try {
    const convs = await listSlackConversations(types, teamId);
    return res.json(convs);
  } catch (e) {
    const msg = (e as Error).message || "";
    // Orphan-state recovery: the Rust CLI's "registered ... but its Keychain
    // slot is missing" diagnostic means a stale workspace row is blocking
    // reconnect. Auto-cleanup so the user can hit Connect and start fresh.
    const isOrphan = msg.includes("Keychain slot is missing") ||
      msg.includes("Keychain slot is missing or unreadable");
    if (isOrphan && teamId) {
      try {
        await runCli(["slack", "remove-workspace", teamId]);
        return res.status(409).json({
          error:
            "This workspace's credentials are out of sync (legacy bug). " +
            "We've cleared the orphaned row — click 'Connect workspace' to re-OAuth.",
          cleanedUp: true,
        });
      } catch (cleanupErr) {
        return res.status(500).json({
          error:
            "Workspace credentials missing AND auto-cleanup failed: " +
            (cleanupErr as Error).message,
        });
      }
    }
    return res.status(500).json({ error: msg });
  }
});

// --- HTMX API Routes ---

router.get("/api/stats", (_req, res) => {
  const stats = getStats();
  res.render("partials/stats", { stats });
});

router.get("/api/actions", (req, res) => {
  const status = req.query.status as ActionStatus | undefined;
  const platform = req.query.platform as string | undefined;
  const page = parseInt(req.query.page as string) || 1;
  const limit = 20;
  const offset = (page - 1) * limit;

  const resolvedStatus = status === ("all" as any) ? undefined : status;
  const resolvedPlatform = platform === "all" ? undefined : platform;

  const actions = getActions({
    limit,
    offset,
    status: resolvedStatus,
    platform: resolvedPlatform,
  });
  const total = getActionCount(resolvedStatus, resolvedPlatform);
  const totalPages = Math.ceil(total / limit);

  res.render("partials/action-rows", {
    actions,
    currentPage: page,
    totalPages,
    currentStatus: status || "all",
    currentPlatform: platform || "all",
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

router.post("/api/config", async (req, res) => {
  const { key, value } = req.body;
  const allowedKeys = [
    "groq_api_key",
    "cerebras_api_key",
    "composio_api_key",
    "discord_webhook_url",
    "discord_bot_token",
    "email_retention_days",
    "github_webhook_secret",
    // #249 — HMAC secret for the SocialAPI.ai inbound webhook receiver
    // (POST /webhooks/socialapi). The receiver reads config-then-env and
    // FAILS CLOSED when neither is set, so configuring this here arms the
    // near-real-time inbox without an env/service restart.
    "socialapi_webhook_secret",
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

  const configStatus = await getConfigStatus();

  res.render("partials/config-status", { configStatus });
});

router.get("/api/config/status", async (_req, res) => {
  const configStatus = await getConfigStatus();
  res.render("partials/config-status", { configStatus });
});

// Ad-hoc agent query. The agent answers with its tools (e.g. meetup_events)
// instead of running email triage. Example:
//   curl -sX POST localhost:<port>/api/ask -H 'content-type: application/json' \
//     -d '{"question":"upcoming C&C events on meetup"}'
router.post("/api/ask", async (req, res) => {
  const question = (req.body?.question ?? "").toString().trim();
  if (!question) {
    res.status(400).json({ error: "question is required" });
    return;
  }
  try {
    const answer = await runAgentQuery(question);
    res.json({ answer });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    console.error("/api/ask failed:", message);
    res.status(500).json({ error: message });
  }
});

// --- Composio OAuth (same pattern as Orchid) ---

/**
 * Generate a unique entity ID for each Gmail account connection.
 * Composio scopes connections per entity_id — unique IDs allow multiple accounts.
 */
function generateEntityId(): string {
  return `augmentagent-${Date.now()}`;
}

/**
 * Find or create a Composio auth config for a toolkit.
 * Composio requires a real auth_config_id UUID, not a toolkit slug.
 */
async function getOrCreateAuthConfig(
  client: Composio,
  toolkit: string
): Promise<string> {
  // Check DB cache first
  const cached = getConfig(`auth_config_${toolkit}`);
  if (cached) return cached;

  // Search for existing Composio-managed config
  const configs = await client.authConfigs.list({
    toolkit_slug: toolkit,
    is_composio_managed: true,
  });

  const existing = configs.items[0];
  if (existing) {
    setConfig(`auth_config_${toolkit}`, existing.id);
    return existing.id;
  }

  // Try without the is_composio_managed filter
  const allConfigs = await client.authConfigs.list({ toolkit_slug: toolkit });
  const anyConfig = allConfigs.items[0];
  if (anyConfig) {
    setConfig(`auth_config_${toolkit}`, anyConfig.id);
    return anyConfig.id;
  }

  // Create a new Composio-managed auth config
  console.log(`Creating Composio auth config for ${toolkit}...`);
  const created = await client.authConfigs.create({
    toolkit: { slug: toolkit },
    auth_config: {
      type: "use_composio_managed_auth",
      name: `augmentagent-${toolkit}`,
      credentials: {},
    },
  });

  const authConfigId = (created as any).auth_config?.id || (created as any).id;
  if (!authConfigId) {
    throw new Error("Composio returned no auth config ID");
  }

  setConfig(`auth_config_${toolkit}`, authConfigId);
  console.log(`Created auth config for ${toolkit}: ${authConfigId}`);
  return authConfigId;
}

router.get("/oauth/gmail/start", async (_req, res) => {
  try {
    const client = getComposioClient();
    if (!client) {
      res.status(400).send("Composio API key not configured. Add it in Settings first.");
      return;
    }

    // Get a real auth config UUID (find existing or create)
    const authConfigId = await getOrCreateAuthConfig(client, "gmail");

    // Each account gets a unique entity ID so Composio tracks them separately
    const entityId = generateEntityId();

    const dashboardPort = process.env.DASHBOARD_PORT || "3000";
    const callbackUrl = `http://localhost:${dashboardPort}/oauth/gmail/callback`;

    // Initiate the connection with callback URL
    const linkResponse = await client.link.create({
      user_id: entityId,
      auth_config_id: authConfigId,
      callback_url: callbackUrl,
    });

    console.log("[oauth] link.create response:", JSON.stringify({
      redirect_url: linkResponse.redirect_url ? "present" : "missing",
      connected_account_id: linkResponse.connected_account_id,
      link_token: linkResponse.link_token ? "present" : "missing",
    }));

    if (!linkResponse.redirect_url) {
      throw new Error("No redirect URL returned from Composio");
    }

    // Store pending connection info so callback can finalize
    if (linkResponse.connected_account_id) {
      setConfig("gmail_pending_connection_id", linkResponse.connected_account_id);
      setConfig("gmail_pending_entity_id", entityId);
      console.log(`[oauth] Stored pending: connectionId=${linkResponse.connected_account_id}, entityId=${entityId}`);
    }

    console.log(`[oauth] Gmail OAuth initiated for entity ${entityId}, redirecting to consent...`);
    res.redirect(linkResponse.redirect_url);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[oauth] Gmail OAuth start failed:", msg);
    res.status(500).send(`
      <div class="p-4 bg-gray-950 text-gray-100 min-h-screen">
        <h2 class="text-lg font-semibold text-red-400 mb-2">OAuth Error</h2>
        <p class="text-sm text-gray-300 mb-4">${msg}</p>
        <a href="/settings" class="text-blue-400 hover:underline">Back to Settings</a>
      </div>
    `);
  }
});

router.get("/oauth/gmail/callback", async (req, res) => {
  console.log("[oauth] Callback hit. Query params:", JSON.stringify(req.query));

  try {
    const connectionId = getConfig("gmail_pending_connection_id");
    const entityId = getConfig("gmail_pending_entity_id");
    const client = getComposioClient();

    console.log("[oauth] Pending state:", { connectionId, entityId, hasClient: !!client });

    if (connectionId && entityId && client) {
      // Check connection status — may need a brief delay for Composio to finalize
      let status = "unknown";
      let email: string | null = null;
      let retries = 3;

      while (retries > 0) {
        try {
          const account = await client.connectedAccounts.retrieve(connectionId);
          status = (account as any).status || "unknown";
          email = (account as any).member_email || (account as any).email || null;
          console.log(`[oauth] Connection ${connectionId} status: ${status}, email: ${email}`);

          if (status === "ACTIVE") break;
        } catch (err) {
          console.log(`[oauth] Retrieve failed (retries left: ${retries - 1}):`, err instanceof Error ? err.message : err);
        }

        retries--;
        if (retries > 0) {
          console.log("[oauth] Waiting 2s before retry...");
          await new Promise((r) => setTimeout(r, 2000));
        }
      }

      // Store the account regardless — we'll check status on poll
      addGmailAccount(connectionId, entityId, email || undefined, email || `Connection (${status})`);
      console.log(`[oauth] Gmail account stored: connectionId=${connectionId}, status=${status}, email=${email}`);

      // Clean up pending state
      deleteConfig("gmail_pending_connection_id");
      deleteConfig("gmail_pending_entity_id");
    } else {
      // Callback fired but no pending state — maybe user navigated directly
      // Try to find new connections by listing all for this app
      console.log("[oauth] No pending state found. Attempting to discover connections...");

      if (client) {
        try {
          const connections = await client.connectedAccounts.list({
            toolkit_slugs: ["gmail"],
          });

          console.log(`[oauth] Found ${connections.items.length} total Gmail connections`);

          // Check if any ACTIVE connections are not in our DB
          const existingAccounts = getGmailAccounts();
          const existingConnectionIds = new Set(existingAccounts.map((a) => a.connectionId));

          for (const conn of connections.items) {
            if (conn.status === "ACTIVE" && !existingConnectionIds.has(conn.id)) {
              const email = (conn as any).member_email || (conn as any).email || null;
              const userId = (conn as any).user_id || (conn as any).entity_id || `discovered-${Date.now()}`;
              addGmailAccount(conn.id, userId, email || undefined, email || "Discovered account");
              console.log(`[oauth] Discovered new Gmail connection: ${conn.id}, email: ${email}`);
            }
          }
        } catch (err) {
          console.error("[oauth] Discovery failed:", err instanceof Error ? err.message : err);
        }
      }
    }

    res.redirect("/settings?gmail=connected");
  } catch (err) {
    console.error("[oauth] Gmail OAuth callback error:", err);
    res.redirect("/settings?gmail=error");
  }
});

// Poll endpoint for frontend to check connection status
router.get("/api/oauth/gmail/status", (_req, res) => {
  const accounts = getActiveGmailAccounts();
  res.json({
    isConnected: accounts.length > 0,
    accountCount: accounts.length,
    accounts: accounts.map((a) => ({
      id: a.id,
      email: a.email,
      entityId: a.entityId,
    })),
  });
});

router.delete("/api/oauth/gmail/:id", async (req, res) => {
  try {
    const client = getComposioClient();
    const accounts = getGmailAccounts();
    const account = accounts.find((a) => a.id === req.params.id);

    if (account && client) {
      try {
        await client.connectedAccounts.delete(account.connectionId);
      } catch {
        // Ignore — may already be deleted on Composio side
      }
    }

    removeGmailAccount(req.params.id);
  } catch {
    removeGmailAccount(req.params.id);
  }

  const configStatus = await getConfigStatus();
  res.render("partials/config-status", { configStatus });
});

// --- Composio OAuth for Google Calendar (#398/#400) ---
// Same shape as the Gmail/Drive flows with one deliberate difference: the
// connection REUSES an existing Gmail account's entity id instead of
// minting a fresh one. The Rust daemon resolves calendar connections by
// iterating gmail_accounts entity ids (Phase 1 reuses them as the Calendar
// entity list), so a fresh entity would leave the connection orphaned.
// One click per account = one fresh single-use consent link per account,
// which is also why connecting two Googles through one link fails with
// Composio's "expired state" redis error.

router.get("/oauth/calendar/start", async (req, res) => {
  try {
    const client = getComposioClient();
    if (!client) {
      res.status(400).send("Composio API key not configured. Add it in Settings first.");
      return;
    }
    const entityId = String(req.query.entity || "");
    const account = getGmailAccounts().find((a) => a.entityId === entityId);
    if (!account) {
      res
        .status(400)
        .send("Unknown entity id — connect the Gmail account first, then its calendar.");
      return;
    }

    const authConfigId = await getOrCreateAuthConfig(client, "googlecalendar");
    const dashboardPort = process.env.DASHBOARD_PORT || "3000";
    const callbackUrl = `http://localhost:${dashboardPort}/oauth/calendar/callback`;
    const linkResponse = await client.link.create({
      user_id: entityId,
      auth_config_id: authConfigId,
      callback_url: callbackUrl,
    });
    if (!linkResponse.redirect_url) {
      throw new Error("No redirect URL returned from Composio");
    }
    if (linkResponse.connected_account_id) {
      setConfig("calendar_pending_connection_id", linkResponse.connected_account_id);
      setConfig("calendar_pending_entity_id", entityId);
    }
    console.log(
      `[oauth] Calendar OAuth initiated for entity ${entityId} (${account.email || "unlabeled"})`,
    );
    res.redirect(linkResponse.redirect_url);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[oauth] Calendar OAuth start failed:", msg);
    res.status(500).send(`
      <div class="p-4 bg-gray-950 text-gray-100 min-h-screen">
        <h2 class="text-lg font-semibold text-red-400 mb-2">OAuth Error</h2>
        <p class="text-sm text-gray-300 mb-4">${msg}</p>
        <a href="/settings" class="text-blue-400 hover:underline">Back to Settings</a>
      </div>
    `);
  }
});

router.get("/oauth/calendar/callback", async (req, res) => {
  console.log("[oauth] calendar callback. Query:", JSON.stringify(req.query));
  try {
    const connectionId = getConfig("calendar_pending_connection_id");
    const entityId = getConfig("calendar_pending_entity_id");
    const client = getComposioClient();

    let status = "unknown";
    if (connectionId && client) {
      let retries = 3;
      while (retries > 0) {
        try {
          const account = await client.connectedAccounts.retrieve(connectionId);
          status = (account as any).status || "unknown";
          if (status === "ACTIVE") break;
        } catch (err) {
          console.log(
            "[oauth] calendar retrieve failed:",
            err instanceof Error ? err.message : err,
          );
        }
        retries--;
        if (retries > 0) await new Promise((r) => setTimeout(r, 2000));
      }
    }
    // No DB row to write — the daemon finds the connection by entity id.
    deleteConfig("calendar_pending_connection_id");
    deleteConfig("calendar_pending_entity_id");
    calendarStatusCache = null;
    console.log(`[oauth] Calendar callback: entity=${entityId}, status=${status}`);
    res.redirect(status === "ACTIVE" ? "/settings?calendar=connected" : "/settings?calendar=error");
  } catch (err) {
    console.error("[oauth] Calendar OAuth callback error:", err);
    res.redirect("/settings?calendar=error");
  }
});

// Per-account calendar connection status (JSON, mirrors gmail/status).
router.get("/api/oauth/calendar/status", async (_req, res) => {
  const entities = await getCalendarConnectedEntities();
  const accounts = getGmailAccounts();
  res.json({
    accounts: accounts.map((a) => ({
      id: a.id,
      email: a.email,
      entityId: a.entityId,
      calendarConnected: entities.includes(a.entityId),
    })),
  });
});

// --- Composio OAuth for Google Drive (multi-tenant) ---
// Byte-for-byte mirror of the Gmail flow with toolkit "googledrive". Writes to
// the `drive_accounts` table in whatever db this dashboard is pointed at
// (run with AUGMENTAGENT_DB=<tenant db> to connect a tenant's Drive).

router.get("/oauth/googledrive/start", async (_req, res) => {
  try {
    const client = getComposioClient();
    if (!client) {
      res.status(400).send("Composio API key not configured. Add it in Settings first.");
      return;
    }
    const authConfigId = await getOrCreateAuthConfig(client, "googledrive");
    const entityId = generateEntityId();
    const dashboardPort = process.env.DASHBOARD_PORT || "3000";
    const callbackUrl = `http://localhost:${dashboardPort}/oauth/googledrive/callback`;
    const linkResponse = await client.link.create({
      user_id: entityId,
      auth_config_id: authConfigId,
      callback_url: callbackUrl,
    });
    if (!linkResponse.redirect_url) {
      throw new Error("No redirect URL returned from Composio");
    }
    if (linkResponse.connected_account_id) {
      setConfig("gdrive_pending_connection_id", linkResponse.connected_account_id);
      setConfig("gdrive_pending_entity_id", entityId);
    }
    console.log(`[oauth] Google Drive OAuth initiated for entity ${entityId}`);
    res.redirect(linkResponse.redirect_url);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[oauth] Google Drive OAuth start failed:", msg);
    res.status(500).send(`
      <div class="p-4 bg-gray-950 text-gray-100 min-h-screen">
        <h2 class="text-lg font-semibold text-red-400 mb-2">OAuth Error</h2>
        <p class="text-sm text-gray-300 mb-4">${msg}</p>
        <a href="/settings" class="text-blue-400 hover:underline">Back to Settings</a>
      </div>
    `);
  }
});

router.get("/oauth/googledrive/callback", async (req, res) => {
  console.log("[oauth] gdrive callback. Query:", JSON.stringify(req.query));
  try {
    const connectionId = getConfig("gdrive_pending_connection_id");
    const entityId = getConfig("gdrive_pending_entity_id");
    const client = getComposioClient();

    if (connectionId && entityId && client) {
      let status = "unknown";
      let email: string | null = null;
      let retries = 3;
      while (retries > 0) {
        try {
          const account = await client.connectedAccounts.retrieve(connectionId);
          status = (account as any).status || "unknown";
          email = (account as any).member_email || (account as any).email || null;
          if (status === "ACTIVE") break;
        } catch (err) {
          console.log("[oauth] gdrive retrieve failed:", err instanceof Error ? err.message : err);
        }
        retries--;
        if (retries > 0) await new Promise((r) => setTimeout(r, 2000));
      }
      addDriveAccount(connectionId, entityId, email || undefined, email || `Connection (${status})`);
      console.log(`[oauth] Drive account stored: ${connectionId}, status=${status}, email=${email}`);
      deleteConfig("gdrive_pending_connection_id");
      deleteConfig("gdrive_pending_entity_id");
    } else if (client) {
      try {
        const connections = await client.connectedAccounts.list({
          toolkit_slugs: ["googledrive"],
        });
        const existing = new Set(getDriveAccounts().map((a) => a.connection_id));
        for (const conn of connections.items) {
          if (conn.status === "ACTIVE" && !existing.has(conn.id)) {
            const email = (conn as any).member_email || (conn as any).email || null;
            const userId =
              (conn as any).user_id || (conn as any).entity_id || `discovered-${Date.now()}`;
            addDriveAccount(conn.id, userId, email || undefined, email || "Discovered account");
          }
        }
      } catch (err) {
        console.error("[oauth] gdrive discovery failed:", err instanceof Error ? err.message : err);
      }
    }
    res.redirect("/settings?googledrive=connected");
  } catch (err) {
    console.error("[oauth] Google Drive OAuth callback error:", err);
    res.redirect("/settings?googledrive=error");
  }
});

router.get("/api/oauth/googledrive/status", (_req, res) => {
  const accounts = getActiveDriveAccounts();
  res.json({
    isConnected: accounts.length > 0,
    accountCount: accounts.length,
    accounts: accounts.map((a) => ({
      id: a.id,
      email: a.email,
      entityId: a.entity_id,
    })),
  });
});

router.delete("/api/oauth/googledrive/:id", async (req, res) => {
  try {
    const client = getComposioClient();
    const account = getDriveAccounts().find((a) => a.id === req.params.id);
    if (account && client) {
      try {
        await client.connectedAccounts.delete(account.connection_id);
      } catch {
        // Ignore — may already be deleted on Composio side
      }
    }
    removeDriveAccount(req.params.id);
  } catch {
    removeDriveAccount(req.params.id);
  }
  res.json({ ok: true });
});

// --- SocialAPI.ai hosted-first setup (#246) ---
//
// The simplest connect path: the operator pastes their SocialAPI.ai API key,
// we store it in `config` (masked when surfaced), then "sync" hits the hosted
// accounts endpoint and upserts each connected handle into socialapi_accounts.
// Each account row can be enabled/disabled to control what the daemon manages,
// or removed entirely. (The proxied-OAuth flow is a separate issue, #247.)

const SOCIALAPI_BASE = "https://api.social-api.ai/v1";

// Render the SocialAPI Settings card back into the page after a mutation.
function renderSocialApiSection(res: any, error: string | null = null) {
  res.render("partials/socialapi-section", {
    socialApiKeyMasked: maskSecret(getConfig("socialapi_api_key")),
    socialApiAccounts: getSocialApiAccounts(),
    socialApiError: error,
  });
}

// Save (or clear) the SocialAPI.ai API key.
router.post("/api/socialapi/key", (req, res) => {
  const value = (req.body?.value ?? "").toString().trim();
  if (value) {
    setConfig("socialapi_api_key", value);
  } else {
    deleteConfig("socialapi_api_key");
  }
  renderSocialApiSection(res);
});

router.post("/api/socialapi/key/clear", (_req, res) => {
  deleteConfig("socialapi_api_key");
  renderSocialApiSection(res);
});

// Sync accounts: GET <base>/accounts with Bearer auth, upsert each into
// socialapi_accounts. Tolerant of a few common response envelopes
// ({accounts:[...]}, {data:[...]}, or a bare array) and missing fields.
router.post("/api/socialapi/sync", async (_req, res) => {
  const key = getConfig("socialapi_api_key");
  if (!key) {
    renderSocialApiSection(res, "No API key set. Paste your SocialAPI.ai key and save it first.");
    return;
  }
  try {
    const resp = await fetch(`${SOCIALAPI_BASE}/accounts`, {
      headers: { Authorization: `Bearer ${key}`, Accept: "application/json" },
    });
    if (!resp.ok) {
      const detail = resp.status === 401 || resp.status === 403
        ? "Invalid or unauthorized API key."
        : `SocialAPI.ai returned HTTP ${resp.status}.`;
      renderSocialApiSection(res, `Sync failed: ${detail}`);
      return;
    }
    const body: any = await resp.json().catch(() => null);
    const list: any[] = Array.isArray(body)
      ? body
      : Array.isArray(body?.accounts)
        ? body.accounts
        : Array.isArray(body?.data)
          ? body.data
          : [];
    let synced = 0;
    for (const a of list) {
      const id = (a?.id ?? a?.account_id ?? a?.uuid ?? "").toString();
      if (!id) continue;
      const platform = (a?.platform ?? a?.network ?? a?.provider ?? "unknown").toString();
      upsertSocialApiAccount(id, platform, {
        brandId: a?.brand_id ?? a?.brandId ?? null,
        displayName: a?.display_name ?? a?.displayName ?? a?.name ?? null,
        accountHandle: a?.account_handle ?? a?.handle ?? a?.username ?? null,
      });
      synced++;
    }
    renderSocialApiSection(res, synced === 0 ? "Sync succeeded but no accounts were returned." : null);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[socialapi] sync failed:", msg);
    renderSocialApiSection(res, `Sync failed: ${msg}`);
  }
});

// Toggle a connected account active/inactive.
router.patch("/api/socialapi/accounts/:id/toggle", (req, res) => {
  const account = getSocialApiAccounts().find((a) => a.id === req.params.id);
  if (account) {
    setSocialApiAccountActive(account.id, !account.active);
  }
  renderSocialApiSection(res);
});

// Remove a connected account from the local registry.
router.delete("/api/socialapi/accounts/:id", (req, res) => {
  removeSocialApiAccount(req.params.id);
  renderSocialApiSection(res);
});

// --- SocialAPI.ai proxied OAuth connect (#247) ---
//
// Mirrors the Gmail/Composio OAuth shape: /start asks SocialAPI.ai to mint a
// hosted auth URL (POST /v1/accounts/connect → { auth_url }), stashes a snapshot
// of the currently-known account IDs as pending state, and redirects the user to
// the provider consent screen. After consent the provider returns to /callback,
// which resolves the freshly-connected account (preferring callback params, then
// falling back to listing GET /v1/accounts and diffing against the snapshot),
// upserts it into socialapi_accounts so it shows up in /api/v1/oauth/status, and
// clears the pending state.

// Render an OAuth error page consistent with the Gmail/Slack flows.
function socialApiOAuthError(res: any, msg: string) {
  res.status(500).send(`
    <div class="p-4 bg-gray-950 text-gray-100 min-h-screen">
      <h2 class="text-lg font-semibold text-red-400 mb-2">SocialAPI.ai OAuth Error</h2>
      <p class="text-sm text-gray-300 mb-4">${msg}</p>
      <a href="/settings" class="text-blue-400 hover:underline">Back to Settings</a>
    </div>
  `);
}

// Tolerant extractor for a single account object from a SocialAPI.ai payload.
function pickSocialApiAccount(a: any): { id: string; platform: string; brandId: string | null; displayName: string | null; accountHandle: string | null } | null {
  const id = (a?.id ?? a?.account_id ?? a?.uuid ?? "").toString();
  if (!id) return null;
  return {
    id,
    platform: (a?.platform ?? a?.network ?? a?.provider ?? "unknown").toString(),
    brandId: a?.brand_id ?? a?.brandId ?? null,
    displayName: a?.display_name ?? a?.displayName ?? a?.name ?? null,
    accountHandle: a?.account_handle ?? a?.handle ?? a?.username ?? null,
  };
}

router.get("/oauth/socialapi/start", async (_req, res) => {
  try {
    const key = getConfig("socialapi_api_key");
    if (!key) {
      socialApiOAuthError(res, "No SocialAPI.ai API key set. Paste your key in Settings and save it first.");
      return;
    }

    const dashboardPort = process.env.DASHBOARD_PORT || "3000";
    const callbackUrl = `http://localhost:${dashboardPort}/oauth/socialapi/callback`;

    const resp = await fetch(`${SOCIALAPI_BASE}/accounts/connect`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${key}`,
        "Content-Type": "application/json",
        Accept: "application/json",
      },
      body: JSON.stringify({ redirect_uri: callbackUrl }),
    });

    if (!resp.ok) {
      const detail = resp.status === 401 || resp.status === 403
        ? "Invalid or unauthorized API key."
        : `SocialAPI.ai returned HTTP ${resp.status}.`;
      socialApiOAuthError(res, `Connect failed: ${detail}`);
      return;
    }

    const body: any = await resp.json().catch(() => null);
    const authUrl = (body?.auth_url ?? body?.authUrl ?? body?.url ?? body?.redirect_url ?? "").toString();
    if (!authUrl) {
      socialApiOAuthError(res, "SocialAPI.ai did not return an auth_url.");
      return;
    }

    // Stash a snapshot of existing account IDs so the callback can diff to find
    // the newly-connected one. Also persist any connect-token the API hands back.
    setConfig("socialapi_pending_known_ids", JSON.stringify(getSocialApiAccounts().map((a) => a.id)));
    const connectToken = (body?.connect_token ?? body?.token ?? body?.id ?? "").toString();
    if (connectToken) {
      setConfig("socialapi_pending_connect_token", connectToken);
    } else {
      deleteConfig("socialapi_pending_connect_token");
    }

    console.log("[socialapi] OAuth connect initiated, redirecting to provider consent...");
    res.redirect(authUrl);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[socialapi] OAuth start failed:", msg);
    socialApiOAuthError(res, msg);
  }
});

router.get("/oauth/socialapi/callback", async (req, res) => {
  console.log("[socialapi] OAuth callback hit. Query params:", JSON.stringify(req.query));
  const key = getConfig("socialapi_api_key");
  if (!key) {
    deleteConfig("socialapi_pending_known_ids");
    deleteConfig("socialapi_pending_connect_token");
    res.redirect("/settings?socialapi=error");
    return;
  }

  try {
    let recorded = false;

    // 1) Prefer an account id handed back directly on the callback URL.
    const directId = (req.query.account_id ?? req.query.id ?? "").toString();
    if (directId) {
      // Try to enrich via GET /v1/accounts; fall back to bare id if it fails.
      let enriched = false;
      try {
        const resp = await fetch(`${SOCIALAPI_BASE}/accounts`, {
          headers: { Authorization: `Bearer ${key}`, Accept: "application/json" },
        });
        if (resp.ok) {
          const body: any = await resp.json().catch(() => null);
          const list: any[] = Array.isArray(body) ? body : (body?.accounts ?? body?.data ?? []);
          const match = list.map(pickSocialApiAccount).find((a) => a && a.id === directId);
          if (match) {
            upsertSocialApiAccount(match.id, match.platform, {
              brandId: match.brandId,
              displayName: match.displayName,
              accountHandle: match.accountHandle,
            });
            recorded = true;
            enriched = true;
          }
        }
      } catch (err) {
        console.error("[socialapi] callback enrich failed:", err instanceof Error ? err.message : err);
      }
      if (!enriched) {
        upsertSocialApiAccount(directId, "unknown", {});
        recorded = true;
      }
    }

    // 2) Fallback discovery: list accounts and diff against the pre-connect snapshot.
    if (!recorded) {
      let knownIds: string[] = [];
      try {
        knownIds = JSON.parse(getConfig("socialapi_pending_known_ids") || "[]");
      } catch {
        knownIds = [];
      }
      const knownSet = new Set(knownIds);

      const resp = await fetch(`${SOCIALAPI_BASE}/accounts`, {
        headers: { Authorization: `Bearer ${key}`, Accept: "application/json" },
      });
      if (resp.ok) {
        const body: any = await resp.json().catch(() => null);
        const list: any[] = Array.isArray(body) ? body : (body?.accounts ?? body?.data ?? []);
        for (const raw of list) {
          const acct = pickSocialApiAccount(raw);
          if (acct && !knownSet.has(acct.id)) {
            upsertSocialApiAccount(acct.id, acct.platform, {
              brandId: acct.brandId,
              displayName: acct.displayName,
              accountHandle: acct.accountHandle,
            });
            recorded = true;
            console.log(`[socialapi] discovered new account via diff: ${acct.id}`);
          }
        }
      } else {
        const detail = resp.status === 401 || resp.status === 403
          ? "Invalid or unauthorized API key."
          : `HTTP ${resp.status}`;
        console.error(`[socialapi] callback list failed: ${detail}`);
      }
    }

    deleteConfig("socialapi_pending_known_ids");
    deleteConfig("socialapi_pending_connect_token");
    res.redirect(recorded ? "/settings?socialapi=connected" : "/settings?socialapi=error");
  } catch (err) {
    console.error("[socialapi] OAuth callback error:", err instanceof Error ? err.message : err);
    deleteConfig("socialapi_pending_known_ids");
    deleteConfig("socialapi_pending_connect_token");
    res.redirect("/settings?socialapi=error");
  }
});

// --- Composio OAuth for Slack (multi-workspace) ---
// Mirrors the Gmail flow: start → Composio hosted consent → callback polls for
// ACTIVE status → shell to Rust CLI to persist auth in Keychain + DB.

router.get("/oauth/slack/start", async (_req, res) => {
  try {
    const client = getComposioClient();
    if (!client) {
      res.status(400).send("Composio API key not configured. Add it in Settings first.");
      return;
    }

    const authConfigId = await getOrCreateAuthConfig(client, "slack");
    const entityId = generateEntityId();

    const dashboardPort = process.env.DASHBOARD_PORT || "3000";
    const callbackUrl = `http://localhost:${dashboardPort}/oauth/slack/callback`;

    const linkResponse = await client.link.create({
      user_id: entityId,
      auth_config_id: authConfigId,
      callback_url: callbackUrl,
    });

    if (!linkResponse.redirect_url) {
      throw new Error("No redirect URL returned from Composio");
    }

    if (linkResponse.connected_account_id) {
      setConfig("slack_pending_connection_id", linkResponse.connected_account_id);
      setConfig("slack_pending_entity_id", entityId);
    }

    console.log(`[oauth] Slack OAuth initiated for entity ${entityId}, redirecting to consent...`);
    res.redirect(linkResponse.redirect_url);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[oauth] Slack OAuth start failed:", msg);
    res.status(500).send(`
      <div class="p-4 bg-gray-950 text-gray-100 min-h-screen">
        <h2 class="text-lg font-semibold text-red-400 mb-2">OAuth Error</h2>
        <p class="text-sm text-gray-300 mb-4">${msg}</p>
        <a href="/subscriptions" class="text-blue-400 hover:underline">Back to Subscriptions</a>
      </div>
    `);
  }
});

router.get("/oauth/slack/callback", async (req, res) => {
  console.log("[oauth] Slack callback hit. Query params:", JSON.stringify(req.query));
  try {
    const connectionId = getConfig("slack_pending_connection_id");
    const entityId = getConfig("slack_pending_entity_id");
    const client = getComposioClient();
    const composioApiKey =
      getConfig("composio_api_key") || process.env.COMPOSIO_API_KEY || "";

    if (!connectionId || !entityId || !client) {
      res.redirect("/subscriptions?slack=error&reason=missing_pending_state");
      return;
    }

    // Mirror Orchid: only verify Composio reports ACTIVE. Don't try to dig
    // team metadata out of connection_data — Rust will call
    // SLACK_FETCH_TEAM_INFO at persist time to learn it. This makes the
    // callback resilient to Composio shape changes.
    let status = "unknown";
    let retries = 4;
    while (retries > 0) {
      try {
        const account = (await client.connectedAccounts.retrieve(connectionId)) as any;
        status = account.status || "unknown";
        if (status === "ACTIVE") break;
      } catch (err) {
        console.log(
          "[oauth] slack retrieve failed (retries left:",
          retries - 1,
          "):",
          err instanceof Error ? err.message : err
        );
      }
      retries--;
      if (retries > 0) await new Promise((r) => setTimeout(r, 2000));
    }

    if (status !== "ACTIVE") {
      console.error(`[oauth] Slack connection not active: status=${status}`);
      res.redirect(`/subscriptions?slack=error&reason=${encodeURIComponent(status)}`);
      return;
    }

    // Hand off to Rust: probes SLACK_FETCH_TEAM_INFO to learn team_id +
    // team_name, persists Keychain slot at augmentagent/slack/<team_id>,
    // upserts the slack_workspaces row.
    const persistResult = await persistSlackAuth({
      entityId,
      connectionId,
      composioApiKey,
    });

    deleteConfig("slack_pending_connection_id");
    deleteConfig("slack_pending_entity_id");

    if (!persistResult.ok) {
      console.error("[oauth] Slack persist failed:", persistResult.error);
      res.redirect(
        `/subscriptions?slack=error&reason=${encodeURIComponent(persistResult.error || "persist_failed")}`
      );
      return;
    }

    console.log(
      `[oauth] Slack workspace connected: team_id=${persistResult.team_id} (${persistResult.team_name})`
    );
    res.redirect("/subscriptions?slack=connected");
  } catch (err) {
    console.error("[oauth] Slack OAuth callback error:", err);
    res.redirect("/subscriptions?slack=error");
  }
});

router.get("/api/slack/workspaces", (_req, res) => {
  const rows = getActiveSlackWorkspaces();
  res.json(
    rows.map((w) => ({
      id: w.id,
      team_id: w.teamId,
      team_name: w.teamName,
      user_id: w.userId,
    }))
  );
});

// Channels for a workspace, annotated with each one's current subscription
// (if any) so the bulk-select UI can pre-check subscribed rows and show mode.
router.get("/api/slack/workspaces/:teamId/channels", async (req, res) => {
  const teamId = req.params.teamId;
  const types = (req.query.types as string | undefined) ||
    "public_channel,private_channel,im,mpim";
  if (!getSlackWorkspaceByTeam(teamId)) {
    return res.status(404).json({ error: "unknown workspace" });
  }
  try {
    const convs = await listSlackConversations(types, teamId);
    // Find existing subscriptions for this workspace so we can mark each
    // channel as already-watched (and surface its mode for context).
    const existing = listSubscriptions("slack", false).filter(
      (s) => s.account_id === teamId
    );
    const subByChannel = new Map(existing.map((s) => [s.channel_id, s]));
    return res.json(
      convs.map((c) => {
        const sub = subByChannel.get(c.id);
        return {
          id: c.id,
          display_name: c.display_name,
          is_im: c.is_im,
          is_mpim: c.is_mpim,
          is_private: c.is_private,
          subscribed: !!sub && sub.active,
          subscription_id: sub?.id ?? null,
          mode: sub?.mode ?? null,
        };
      })
    );
  } catch (e) {
    const msg = (e as Error).message || "";
    if (msg.includes("Keychain slot is missing")) {
      try {
        await runCli(["slack", "remove-workspace", teamId]);
        return res.status(409).json({
          error:
            "This workspace's credentials are out of sync. We've cleared " +
            "the orphan row — click 'Connect workspace' to re-OAuth.",
          cleanedUp: true,
        });
      } catch {
        // fall through to generic error
      }
    }
    return res.status(500).json({ error: msg });
  }
});

// Bulk subscribe / unsubscribe for a workspace. Body shape:
// { mode: "priority"|"digest"|"store_only", channels: [{ id, display_name }] }
// — upserts each, leaves channels not in the list alone unless `replace=true`,
// in which case any current sub on this workspace not in the list gets soft-deleted.
router.post("/api/slack/workspaces/:teamId/subscribe", (req, res) => {
  const teamId = req.params.teamId;
  if (!getSlackWorkspaceByTeam(teamId)) {
    return res.status(404).json({ error: "unknown workspace" });
  }
  const { mode, channels, replace } = req.body as {
    mode?: string;
    channels?: { id: string; display_name: string }[];
    replace?: boolean;
  };
  if (!mode || !ALLOWED_MODES.includes(mode as SubscriptionMode)) {
    return res.status(400).json({ error: "invalid mode" });
  }
  if (!Array.isArray(channels)) {
    return res.status(400).json({ error: "channels[] required" });
  }
  const wantIds = new Set(channels.map((c) => c.id));
  for (const ch of channels) {
    if (!ch.id || !ch.display_name) continue;
    upsertSubscription("slack", ch.id, ch.display_name, mode as SubscriptionMode, teamId);
  }
  if (replace) {
    const existing = listSubscriptions("slack", true).filter((s) => s.account_id === teamId);
    for (const sub of existing) {
      if (!wantIds.has(sub.channel_id)) {
        deleteSubscription(sub.id);
      }
    }
  }
  const subs = listSubscriptions();
  return res.render("partials/subscription-rows", { subs });
});

router.delete("/api/slack/workspaces/:teamId", async (req, res) => {
  const teamId = req.params.teamId;
  if (!getSlackWorkspaceByTeam(teamId)) {
    return res.status(404).json({ error: "unknown workspace" });
  }
  // Best-effort: tell Rust to drop the Keychain slot + deactivate the row.
  try {
    await runCli(["slack", "remove-workspace", teamId]);
  } catch (e) {
    // If the CLI call failed, still flip the DB row so the dashboard is consistent.
    deactivateSlackWorkspace(teamId);
    console.warn(`[slack] remove-workspace CLI failed, DB deactivated anyway: ${(e as Error).message}`);
  }
  return res.json({ ok: true });
});

// --- GitHub Webhook (auto-update on push) ---

function verifyGitHubSignature(payload: string, signature: string | undefined, secret: string): boolean {
  if (!signature) return false;
  const expected = "sha256=" + crypto.createHmac("sha256", secret).update(payload).digest("hex");
  return crypto.timingSafeEqual(Buffer.from(signature), Buffer.from(expected));
}

let updateInProgress = false;

router.post("/api/webhook/github", (req, res) => {
  const secret = getConfig("github_webhook_secret") || process.env.GITHUB_WEBHOOK_SECRET;
  if (!secret) {
    res.status(500).json({ error: "GITHUB_WEBHOOK_SECRET not configured" });
    return;
  }

  // Verify signature
  const body = JSON.stringify(req.body);
  const signature = req.headers["x-hub-signature-256"] as string | undefined;
  if (!verifyGitHubSignature(body, signature, secret)) {
    console.warn("[webhook] Invalid GitHub signature — rejected");
    res.status(403).json({ error: "Invalid signature" });
    return;
  }

  // Only act on pushes to main
  const ref = req.body?.ref as string | undefined;
  if (ref !== "refs/heads/main") {
    console.log(`[webhook] Push to ${ref} — ignoring (not main)`);
    res.json({ status: "ignored", reason: "not main branch" });
    return;
  }

  if (updateInProgress) {
    console.log("[webhook] Update already in progress — skipping");
    res.json({ status: "skipped", reason: "update already in progress" });
    return;
  }

  // Respond immediately, run update in background
  res.json({ status: "updating" });
  updateInProgress = true;

  const pusher = req.body?.pusher?.name || "unknown";
  console.log(`[webhook] Push to main by ${pusher} — starting update...`);

  const updateCmd = [
    "git pull origin main",
    "npm install --production",
    "npm run build",
    "pm2 restart augmentagent",
  ].join(" && ");

  exec(updateCmd, { cwd: process.cwd() }, (err, stdout, stderr) => {
    updateInProgress = false;
    if (err) {
      console.error("[webhook] Update failed:", err.message);
      if (stderr) console.error("[webhook] stderr:", stderr);
    } else {
      console.log("[webhook] Update complete. Output:", stdout);
    }
  });
});

// --- Resume ingestion ---
// One-shot seeding of the wiki from the user's CV. Hands the uploaded file to
// the Rust CLI's `resume ingest` subcommand which drives a Claude call that
// writes `about/me.md` + stub `people/<slug>.md` pages.

const RESUME_TMP_DIR = path.join(os.tmpdir(), "augmentagent-resume");
const resumeUpload = multer({
  dest: RESUME_TMP_DIR,
  limits: { fileSize: 20 * 1024 * 1024 }, // 20 MB cap
  fileFilter: (_req, file, cb) => {
    const ext = path.extname(file.originalname).toLowerCase();
    if ([".pdf", ".txt", ".md"].includes(ext)) {
      cb(null, true);
    } else {
      cb(new Error(`unsupported extension ${ext}; use .pdf, .txt, or .md`));
    }
  },
});

router.get("/resume", (_req, res) => {
  res.render("resume", { page: "resume" });
});

router.post("/api/resume", resumeUpload.single("resume"), (req, res) => {
  const file = (req as Request & { file?: Express.Multer.File }).file;
  if (!file) {
    res.status(400).send("no file uploaded");
    return;
  }

  // multer gives us a tmp path without the original extension. Rename to
  // preserve extension so the Rust CLI picks the right parser.
  const originalExt = path.extname(file.originalname).toLowerCase();
  const finalPath = path.join(RESUME_TMP_DIR, `resume-${Date.now()}${originalExt}`);
  try {
    fs.renameSync(file.path, finalPath);
  } catch (e) {
    res.status(500).send(`failed to stage resume file: ${(e as Error).message}`);
    return;
  }

  const repoRoot = process.cwd();
  const wikiDir = process.env.AUGMENTAGENT_WIKI_DIR || path.join(repoRoot, "wiki");
  const binPath = path.join(repoRoot, "target/release/augmentagent");

  const args = ["--wiki-dir", wikiDir, "resume", "ingest", "--file", finalPath];
  const child = spawn(binPath, args, { cwd: repoRoot });

  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (d) => (stdout += d.toString()));
  child.stderr.on("data", (d) => (stderr += d.toString()));

  // Generous ceiling — Opus ingest of a full CV can take ~60-90s.
  const timeout = setTimeout(() => {
    child.kill("SIGKILL");
  }, 180_000);

  child.on("close", (code) => {
    clearTimeout(timeout);
    try {
      fs.unlinkSync(finalPath);
    } catch {
      /* best-effort cleanup */
    }

    if (code !== 0) {
      res.status(500).json({ ok: false, code, stderr, stdout });
      return;
    }

    // Claude ends its response with `wrote: path1, path2, ...`. Pull that line out.
    const wroteLine =
      stdout
        .split("\n")
        .map((l) => l.trim())
        .reverse()
        .find((l) => l.toLowerCase().startsWith("wrote:")) || "(no wrote: line found)";

    res.json({ ok: true, wrote: wroteLine, stdout });
  });

  child.on("error", (e) => {
    clearTimeout(timeout);
    res.status(500).json({ ok: false, error: e.message });
  });
});

// ---------------------------------------------------------------------------
// #57 — Proactive nudges UX. Server-rendered relationships dashboard.
// The Rust runner persists `proactive_signals`; this surface lets the user
// triage them (Draft / Snooze / Stop tracking) and edit a person's cadence
// (written straight to the wiki person-page frontmatter).
// ---------------------------------------------------------------------------

import fsSync from "fs";
import pathMod from "path";

function wikiRoot(): string {
  return process.env.AUGMENTAGENT_WIKI_DIR || pathMod.join(process.cwd(), "wiki");
}

function personPagePath(slug: string): string {
  // Defensive: slugs are wiki filenames; reject path escapes.
  const safe = slug.replace(/[^a-zA-Z0-9_.-]/g, "_");
  return pathMod.join(wikiRoot(), "people", `${safe}.md`);
}

/**
 * Upsert a single scalar frontmatter key in a person page, preserving the
 * rest of the file byte-for-byte. Mirrors the "never round-trip the whole
 * frontmatter" rule from the wiki migration design.
 */
function setPersonFrontmatterKey(slug: string, key: string, value: string): boolean {
  const fp = personPagePath(slug);
  if (!fsSync.existsSync(fp)) return false;
  const raw = fsSync.readFileSync(fp, "utf8");
  const m = raw.match(/^---\n([\s\S]*?)\n---/);
  if (!m) return false;
  const fmLines = m[1].split("\n");
  const keyRe = new RegExp(`^${key}:\\s*.*$`);
  let replaced = false;
  const out = fmLines.map((l) => {
    if (keyRe.test(l)) {
      replaced = true;
      return `${key}: ${value}`;
    }
    return l;
  });
  if (!replaced) out.push(`${key}: ${value}`);
  const newFm = out.join("\n");
  const newRaw = raw.replace(m[0], `---\n${newFm}\n---`);
  fsSync.writeFileSync(fp, newRaw);
  return true;
}

// ---------------------------------------------------------------------------
// #177 — Sessions tab: enumerate running `claude` processes, with a Kill
// action per row. Frontend lives in views/sessions.ejs and polls /api/sessions
// every 5s while the tab is open. POST /api/sessions/:pid/stop hard-rejects
// any PID whose cmdline doesn't include "claude" as a defense-in-depth check
// before signaling.
// ---------------------------------------------------------------------------

type SessionRow = {
  pid: number;
  ppid: number;
  etime: string;
  tty: string;
  cwd: string;
  cmd: string;
};

function readProcCmdline(pid: number): string[] | null {
  try {
    const raw = fs.readFileSync(`/proc/${pid}/cmdline`);
    // /proc/<pid>/cmdline is NUL-separated and trailing-NUL terminated.
    const parts = raw
      .toString("utf8")
      .split("\0")
      .filter((s) => s.length > 0);
    return parts.length ? parts : null;
  } catch {
    return null;
  }
}

function readProcComm(pid: number): string | null {
  try {
    return fs.readFileSync(`/proc/${pid}/comm`, "utf8").trim();
  } catch {
    return null;
  }
}

function readProcCwd(pid: number): string {
  try {
    return fs.readlinkSync(`/proc/${pid}/cwd`);
  } catch {
    return "";
  }
}

function readProcStatPpid(pid: number): number {
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    // The comm field is wrapped in parens and may contain spaces/parens —
    // split on the LAST ')' so the field offsets line up regardless.
    const close = stat.lastIndexOf(")");
    if (close < 0) return 0;
    const rest = stat.slice(close + 2).split(/\s+/);
    // After comm: state ppid pgrp ... (ppid is index 1 in `rest`)
    return parseInt(rest[1] || "0", 10) || 0;
  } catch {
    return 0;
  }
}

function isClaudeCmdline(cmdline: string[] | null, comm: string | null): boolean {
  if (comm === "claude") return true;
  if (!cmdline || cmdline.length === 0) return false;
  const base = path.basename(cmdline[0]);
  return base === "claude";
}

function enumerateClaudeSessions(): SessionRow[] {
  const rows: SessionRow[] = [];
  let entries: string[] = [];
  try {
    entries = fs.readdirSync("/proc");
  } catch {
    return rows;
  }
  const pids: number[] = [];
  for (const e of entries) {
    if (/^\d+$/.test(e)) pids.push(parseInt(e, 10));
  }

  // Pull etime + tty from a single `ps` invocation keyed by PID. /proc gives
  // us start time in clock ticks since boot — easier and more accurate to let
  // `ps` format the elapsed time string ("etime") and resolve the tty.
  let psMap = new Map<number, { etime: string; tty: string }>();
  if (pids.length) {
    try {
      const { execSync } = require("child_process") as typeof import("child_process");
      const out = execSync("ps -e -o pid=,etime=,tty=", {
        encoding: "utf8",
        timeout: 2000,
      });
      for (const line of out.split("\n")) {
        const m = line.trim().match(/^(\d+)\s+(\S+)\s+(\S+)$/);
        if (!m) continue;
        psMap.set(parseInt(m[1], 10), { etime: m[2], tty: m[3] });
      }
    } catch {
      // ps may be unavailable in some sandboxes; fall through with empty map.
      psMap = new Map();
    }
  }

  for (const pid of pids) {
    const cmdline = readProcCmdline(pid);
    const comm = readProcComm(pid);
    if (!isClaudeCmdline(cmdline, comm)) continue;
    const ppid = readProcStatPpid(pid);
    const cwd = readProcCwd(pid);
    const meta = psMap.get(pid);
    rows.push({
      pid,
      ppid,
      etime: meta?.etime ?? "",
      tty: meta?.tty ?? "?",
      cwd,
      cmd: (cmdline || [comm || "claude"]).join(" "),
    });
  }
  // Stable order: youngest first by PID (proxy for newest), like a process list.
  rows.sort((a, b) => b.pid - a.pid);
  return rows;
}

router.get("/sessions", (_req, res) => {
  res.render("sessions", { page: "sessions" });
});

router.get("/api/sessions", (_req, res) => {
  try {
    res.json(enumerateClaudeSessions());
  } catch (e) {
    res.status(500).json({ error: (e as Error).message });
  }
});

router.post("/api/sessions/:pid/stop", (req, res) => {
  const pid = parseInt(req.params.pid, 10);
  if (!Number.isFinite(pid) || pid <= 1) {
    res.status(400).json({ error: "invalid pid" });
    return;
  }
  // Defense-in-depth: re-read /proc/<pid>/cmdline at signal time and
  // hard-reject anything that doesn't look like a claude process. Prevents
  // a stale client (or a malicious one) from convincing us to SIGKILL init
  // or some unrelated daemon that happened to recycle the PID.
  const cmdline = readProcCmdline(pid);
  const comm = readProcComm(pid);
  if (!isClaudeCmdline(cmdline, comm)) {
    res.status(403).json({
      error: "pid is not a claude process (refusing to signal)",
    });
    return;
  }
  const force = !!(req.body && req.body.force);
  const signal: NodeJS.Signals = force ? "SIGKILL" : "SIGTERM";
  try {
    process.kill(pid, signal);
    res.json({ ok: true, pid, signal });
  } catch (e) {
    res.status(500).json({ error: (e as Error).message });
  }
});

// #132 — "Query agent activity" view. Tails the tool-audit NDJSON log the
// Rust reasoner writes for each tool call its wiki-query agent makes. The
// view itself is in views/audit.ejs and polls /api/audit every 5s.
//
// We read the tail of the file (last ~256 KB) and parse line-by-line newest
// first, so a runaway log doesn't blow the dashboard's memory.
const TOOL_AUDIT_LOG_PATH =
  process.env.AUGMENTAGENT_TOOL_AUDIT_LOG ||
  path.join(
    process.env.HOME || "/tmp",
    ".local/state/augmentagent/tool-audit.log"
  );
const AUDIT_TAIL_BYTES = 256 * 1024;

function readAuditTail(limit: number): Array<Record<string, unknown>> {
  let stat: fs.Stats;
  try {
    stat = fs.statSync(TOOL_AUDIT_LOG_PATH);
  } catch (_e) {
    return []; // log not created yet — fine.
  }
  const start = Math.max(0, stat.size - AUDIT_TAIL_BYTES);
  const buf = Buffer.alloc(stat.size - start);
  const fd = fs.openSync(TOOL_AUDIT_LOG_PATH, "r");
  try {
    fs.readSync(fd, buf, 0, buf.length, start);
  } finally {
    fs.closeSync(fd);
  }
  // If we sliced mid-line, drop the leading partial line.
  let text = buf.toString("utf8");
  if (start > 0) {
    const nl = text.indexOf("\n");
    if (nl >= 0) text = text.slice(nl + 1);
  }
  const lines = text.split("\n").filter((l) => l.trim().length > 0);
  // Newest first.
  lines.reverse();
  const out: Array<Record<string, unknown>> = [];
  for (const line of lines) {
    if (out.length >= limit) break;
    try {
      out.push(JSON.parse(line) as Record<string, unknown>);
    } catch (_e) {
      // Skip malformed lines (truncated write, partial flush) silently.
    }
  }
  return out;
}

router.get("/audit", (_req, res) => {
  res.render("audit", { page: "audit" });
});

router.get("/api/audit", (req, res) => {
  const rawLimit = parseInt(String(req.query.limit ?? "200"), 10);
  const limit = Number.isFinite(rawLimit) ? Math.max(1, Math.min(rawLimit, 1000)) : 200;
  try {
    const rows = readAuditTail(limit);
    res.json({ path: TOOL_AUDIT_LOG_PATH, rows });
  } catch (e) {
    res.status(500).json({ error: (e as Error).message });
  }
});

router.get("/relationships", (_req, res) => {
  const signals = listProactiveSignals(200);
  const actions = listActiveProactiveUserActions();
  const mutedPeople = new Set(
    actions.filter((a) => a.action === "mute_person").map((a) => a.scope)
  );
  const mutedRules = new Set(
    actions.filter((a) => a.action === "mute_rule").map((a) => a.scope)
  );
  res.render("relationships", {
    signals,
    mutedPeople: [...mutedPeople],
    mutedRules: [...mutedRules],
    page: "relationships",
  });
});

router.get("/relationships/:slug", (req, res) => {
  const slug = req.params.slug;
  const signals = listProactiveSignalsForPerson(slug);
  const actions = listActiveProactiveUserActions();
  const muted = actions.some(
    (a) => a.action === "mute_person" && a.scope === slug
  );
  let pageExists = false;
  let cadence = "";
  try {
    const fp = personPagePath(slug);
    if (fsSync.existsSync(fp)) {
      pageExists = true;
      const raw = fsSync.readFileSync(fp, "utf8");
      const cm = raw.match(/^cadence:\s*(.+)$/m);
      if (cm) cadence = cm[1].trim();
    }
  } catch {
    /* best-effort */
  }
  res.render("relationship-detail", {
    slug,
    signals,
    muted,
    pageExists,
    cadence,
    page: "relationships",
  });
});

// Inline actions. All redirect back so the page reflects new state (no SPA).
router.post("/relationships/:id/draft", (req, res) => {
  // Phase-1: surface the suggested draft prompt. Wiring it into the drafter
  // queue is deferred (Refs #57) — for now we echo the prompt so the user
  // can paste it into the wiki-ask box. Keeps the UX honest about scope.
  const sig = getProactiveSignal(req.params.id);
  res.render("relationship-draft", {
    signal: sig,
    page: "relationships",
  });
});

router.post("/relationships/:id/snooze", (req, res) => {
  const days = parseInt((req.body.days as string) || "7", 10) || 7;
  const sig = getProactiveSignal(req.params.id);
  snoozeProactiveSignal(req.params.id, days);
  if (sig) recordProactiveUserAction("snooze", sig.dedup_key, days);
  res.redirect("/relationships");
});

router.post("/relationships/:id/dismiss", (req, res) => {
  const sig = getProactiveSignal(req.params.id);
  dismissProactiveSignal(req.params.id);
  if (sig) recordProactiveUserAction("dismiss", sig.dedup_key);
  res.redirect("/relationships");
});

router.post("/relationships/person/:slug/mute", (req, res) => {
  recordProactiveUserAction("mute_person", req.params.slug);
  res.redirect(`/relationships/${encodeURIComponent(req.params.slug)}`);
});

router.post("/relationships/person/:slug/unmute", (req, res) => {
  clearProactiveUserAction("mute_person", req.params.slug);
  res.redirect(`/relationships/${encodeURIComponent(req.params.slug)}`);
});

router.post("/relationships/person/:slug/cadence", (req, res) => {
  const cadence = (req.body.cadence as string || "").trim();
  const allowed = ["weekly", "bi-weekly", "monthly", "quarterly", "ad-hoc"];
  if (allowed.includes(cadence)) {
    setPersonFrontmatterKey(req.params.slug, "cadence", cadence);
  }
  res.redirect(`/relationships/${encodeURIComponent(req.params.slug)}`);
});

router.post("/relationships/rule/:kind/mute", (req, res) => {
  recordProactiveUserAction("mute_rule", req.params.kind);
  res.redirect("/relationships");
});

router.post("/relationships/rule/:kind/unmute", (req, res) => {
  clearProactiveUserAction("mute_rule", req.params.kind);
  res.redirect("/relationships");
});

export default router;
