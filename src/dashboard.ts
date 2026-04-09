import { Router } from "express";
import { exec } from "child_process";
import crypto from "crypto";
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
  hasAnyGmailAccount,
  getEmailCount,
  purgeOldEmails,
} from "./db";
import type { ActionStatus } from "./types";

const router = Router();

function getComposioClient(): Composio | null {
  const apiKey = getConfig("composio_api_key") || process.env.COMPOSIO_API_KEY;
  if (!apiKey) return null;
  return new Composio({ apiKey });
}

// Check both DB config and env vars for integration status
function getConfigStatus() {
  const gmailAccounts = getGmailAccounts();
  return {
    groqKey: !!(getConfig("groq_api_key") || process.env.GROQ_API_KEY),
    cerebrasKey: !!(getConfig("cerebras_api_key") || process.env.CEREBRAS_API_KEY),
    composioKey: !!(getConfig("composio_api_key") || process.env.COMPOSIO_API_KEY),
    gmailAccounts,
    gmailConnected: gmailAccounts.some((a) => a.active),
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
  const configStatus = getConfigStatus();
  const emailRetention = getConfig("email_retention_days") || "0";
  const emailCount = getEmailCount();
  res.render("settings", { senders, configStatus, emailRetention, emailCount, page: "settings" });
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
    "cerebras_api_key",
    "composio_api_key",
    "discord_webhook_url",
    "discord_bot_token",
    "email_retention_days",
    "github_webhook_secret",
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

  const configStatus = getConfigStatus();

  res.render("partials/config-status", { configStatus });
});

router.get("/api/config/status", (_req, res) => {
  const configStatus = getConfigStatus();
  res.render("partials/config-status", { configStatus });
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

  const configStatus = getConfigStatus();
  res.render("partials/config-status", { configStatus });
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

export default router;
