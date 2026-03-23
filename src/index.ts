import dotenv from "dotenv";
dotenv.config();

import express from "express";
import path from "path";
import { fetchUnreadEmails, sendDraft, createDraft } from "./gmailService";
import { generateReply } from "./llmService";
import { initBot, sendForApproval } from "./discordService";
import {
  initDb,
  logAction,
  updateActionStatus,
  isMessageProcessed,
  getActiveSenders,
  addSender,
  getSenders,
} from "./db";
import dashboardRouter from "./dashboard";

const POLL_INTERVAL_MS = 2 * 60 * 1000; // 2 minutes
const DASHBOARD_PORT = parseInt(process.env.DASHBOARD_PORT || "3000");

function seedSendersFromEnv(): void {
  const envSenders = process.env.IMPORTANT_SENDERS;
  if (!envSenders) return;

  const existing = getSenders().map((s) => s.email);
  const toSeed = envSenders
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.includes("@") && !existing.includes(s.toLowerCase()));

  for (const email of toSeed) {
    addSender(email, "From env");
  }

  if (toSeed.length > 0) {
    console.log(`Seeded ${toSeed.length} sender(s) from IMPORTANT_SENDERS env var.`);
  }
}

async function pollAndProcess(): Promise<void> {
  const senders = getActiveSenders();
  if (senders.length === 0) {
    console.log("No active senders configured. Skipping poll.");
    return;
  }

  console.log(
    `[${new Date().toISOString()}] Polling for emails from ${senders.length} sender(s)...`
  );

  const emails = await fetchUnreadEmails(senders);
  const newEmails = emails.filter((e) => !isMessageProcessed(e.messageId));

  if (newEmails.length === 0) {
    console.log("No new emails found.");
    return;
  }

  console.log(`Found ${newEmails.length} new email(s).`);

  for (const email of newEmails) {
    const actionId = logAction({
      messageId: email.messageId,
      threadId: email.threadId,
      fromEmail: email.from,
      subject: email.subject,
      originalBody: email.body,
      status: "pending",
    });

    try {
      console.log(`Processing: "${email.subject}" from ${email.from}`);

      const draft = await generateReply(email.body);
      updateActionStatus(actionId, "pending", { draftBody: draft });
      console.log("Draft generated. Sending to Discord for approval...");

      const approved = await sendForApproval(email, draft);

      if (approved) {
        updateActionStatus(actionId, "approved");
        const draftId = await createDraft(
          email.from,
          `Re: ${email.subject}`,
          draft,
          email.threadId
        );
        await sendDraft(draftId);
        updateActionStatus(actionId, "sent");
        console.log(`Reply sent to ${email.from}`);
      } else {
        updateActionStatus(actionId, "rejected");
        console.log(`Draft rejected for "${email.subject}"`);
      }
    } catch (err) {
      const errorMsg = err instanceof Error ? err.message : String(err);
      updateActionStatus(actionId, "error", { errorMessage: errorMsg });
      console.error(`Error processing email "${email.subject}":`, err);
    }
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

  // Seed senders from env (first run)
  seedSendersFromEnv();

  // Start web dashboard
  startDashboard();

  // Initialize Discord bot
  try {
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
}

main().catch((err) => {
  console.error("Fatal error:", err);
  process.exit(1);
});
