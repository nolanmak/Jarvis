import dotenv from "dotenv";
dotenv.config();

import { execSync } from "child_process";
import express from "express";
import path from "path";
import { initBot, setRedraftFunction } from "./discordService";
import {
  initDb,
  getActiveGmailAccounts,
  upsertEmail,
  purgeOldEmails,
} from "./db";
import { fetchUnreadEmails } from "./gmailService";
import { runAgent, redraftWithFeedback } from "./agent";
import dashboardRouter from "./dashboard";
import type { Email } from "./types";

const POLL_INTERVAL_MS = 2 * 60 * 1000; // 2 minutes
const UPDATE_CHECK_INTERVAL_MS = 5 * 60 * 1000; // 5 minutes
const DASHBOARD_PORT = parseInt(process.env.DASHBOARD_PORT || "3000");

async function pollAndProcess(): Promise<void> {
  const accounts = getActiveGmailAccounts();
  if (accounts.length === 0) {
    console.log("No Gmail accounts connected. Skipping poll. Connect at http://localhost:3000/settings");
    return;
  }

  // EXTRACT: fetch unread emails from Gmail (cheap API call)
  const allEmails: Email[] = [];
  for (const account of accounts) {
    const emails = await fetchUnreadEmails(account.entityId);
    allEmails.push(...emails.map((e) => ({ ...e, accountEntityId: account.entityId })));
  }

  // LOAD: store in DB, track which are new
  const newEmails: Email[] = [];
  for (const email of allEmails) {
    const isNew = upsertEmail(email, email.accountEntityId!);
    if (isNew) newEmails.push(email);
  }

  // DECIDE: only invoke agent if there are new emails
  if (newEmails.length === 0) {
    console.log(`[${new Date().toISOString()}] No new emails. Skipping agent.`);
    return;
  }

  console.log(
    `[${new Date().toISOString()}] ${newEmails.length} new email(s). Invoking agent...`
  );

  // Build context with email data so agent doesn't need to fetch
  const emailSummaries = newEmails.map((e, i) =>
    `Email ${i + 1}:
  messageId: ${e.messageId}
  threadId: ${e.threadId}
  from: ${e.from}
  subject: ${e.subject}
  date: ${e.date}
  accountEntityId: ${e.accountEntityId}
  body: ${e.body}`
  ).join("\n\n");

  const accountInfo = accounts
    .map((a) => `  - entity_id: ${a.entityId}, email: ${a.email || "unknown"}`)
    .join("\n");

  const dynamicContext = `<context>
Current time: ${new Date().toISOString()}
Connected Gmail accounts:
${accountInfo}
</context>

<new_emails>
${emailSummaries}
</new_emails>

You have ${newEmails.length} new email(s) to triage. The emails are provided above — do NOT fetch emails, just triage each one and process accordingly.`;

  const result = await runAgent(dynamicContext);
  console.log(`Agent completed: ${result}`);

  // Purge old emails if retention policy is set (no-op when "keep forever")
  const purged = purgeOldEmails();
  if (purged > 0) {
    console.log(`[${new Date().toISOString()}] Purged ${purged} expired email(s) per retention policy.`);
  }
}

function startDashboard(): void {
  const app = express();

  app.use(express.json());
  app.use(express.urlencoded({ extended: true }));

  // Static files
  app.use(express.static(path.join(__dirname, "..", "public")));

  // View engine
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "..", "views"));

  // Routes
  app.use(dashboardRouter);

  app.listen(DASHBOARD_PORT, () => {
    console.log(`Dashboard running at http://localhost:${DASHBOARD_PORT}`);
  });
}

async function main(): Promise<void> {
  console.log("AugmentAgent starting...");

  // Initialize database
  initDb();
  console.log("Database initialized.");

  // Start web dashboard
  startDashboard();

  // Initialize Discord bot + wire redraft function
  try {
    setRedraftFunction(redraftWithFeedback);
    await initBot();
    console.log("Discord bot ready.");
  } catch (err) {
    console.warn("Discord bot failed to start (approval via dashboard only):", err);
  }

  // Initial poll
  try {
    await pollAndProcess();
  } catch (err) {
    console.error("Initial poll failed:", err);
  }

  // Start polling loop
  setInterval(() => {
    pollAndProcess().catch((err) => {
      console.error("Polling error:", err);
    });
  }, POLL_INTERVAL_MS);

  console.log(
    `Polling every ${POLL_INTERVAL_MS / 1000}s. Press Ctrl+C to stop.`
  );

  // Start code update checking loop
  setInterval(() => checkForUpdates(), UPDATE_CHECK_INTERVAL_MS);
  console.log(
    `Checking for code updates every ${UPDATE_CHECK_INTERVAL_MS / 1000}s.`
  );
}

function checkForUpdates(): void {
  try {
    const cwd = process.cwd();

    // Fetch latest from remote
    execSync("git fetch origin main", { cwd, stdio: "pipe" });

    // Compare local HEAD vs remote HEAD
    const local = execSync("git rev-parse HEAD", { cwd, stdio: "pipe" }).toString().trim();
    const remote = execSync("git rev-parse origin/main", { cwd, stdio: "pipe" }).toString().trim();

    if (local === remote) {
      console.log(`[${new Date().toISOString()}] Code up to date (${local.slice(0, 7)})`);
      return;
    }

    console.log(`[${new Date().toISOString()}] New commits detected (${local.slice(0, 7)} → ${remote.slice(0, 7)}). Updating...`);
    execSync("git pull origin main && npm install --production && npm run build", { cwd, stdio: "inherit" });

    console.log(`[${new Date().toISOString()}] Build complete. Restarting via pm2...`);
    execSync("pm2 restart augmentagent", { cwd, stdio: "inherit" });
  } catch (err) {
    console.error(`[${new Date().toISOString()}] Update check failed:`, err instanceof Error ? err.message : err);
  }
}

main().catch((err) => {
  console.error("Fatal error:", err);
  process.exit(1);
});
