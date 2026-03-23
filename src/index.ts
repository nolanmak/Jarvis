import dotenv from "dotenv";
dotenv.config();

import express from "express";
import path from "path";
import { initBot } from "./discordService";
import {
  initDb,
  getActiveSenders,
  getRecentProcessedIds,
  addSender,
  getSenders,
} from "./db";
import { runAgent } from "./agent";
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

  // Dynamic context — keeps agent instructions (prefix) stable for caching
  const recentIds = getRecentProcessedIds();
  const dynamicContext = `<context>
Active senders: ${senders.join(", ")}
Current time: ${new Date().toISOString()}
Recently processed message IDs (skip these): ${recentIds.join(", ") || "none"}
</context>

Check for new emails from the tracked senders and process any you find.`;

  console.log(
    `[${new Date().toISOString()}] Running agent for ${senders.length} sender(s)...`
  );

  const result = await runAgent(dynamicContext);
  console.log(`Agent completed: ${result}`);
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
