import dotenv from "dotenv";
import { fetchUnreadEmails, sendDraft, createDraft } from "./gmailService";
import { generateReply } from "./llmService";
import { initBot, sendForApproval } from "./discordService";

dotenv.config();

const POLL_INTERVAL_MS = 2 * 60 * 1000; // 2 minutes
const processedMessageIds = new Set<string>();

function getImportantSenders(): string[] {
  const senders = process.env.IMPORTANT_SENDERS;
  if (!senders) {
    throw new Error("IMPORTANT_SENDERS env var is not set");
  }
  return senders.split(",").map((s) => s.trim());
}

async function pollAndProcess(): Promise<void> {
  const senders = getImportantSenders();
  console.log(`[${new Date().toISOString()}] Polling for emails from ${senders.length} senders...`);

  const emails = await fetchUnreadEmails(senders);
  const newEmails = emails.filter((e) => !processedMessageIds.has(e.messageId));

  if (newEmails.length === 0) {
    console.log("No new emails found.");
    return;
  }

  console.log(`Found ${newEmails.length} new email(s).`);

  for (const email of newEmails) {
    try {
      console.log(`Processing: "${email.subject}" from ${email.from}`);

      const draft = await generateReply(email.body);
      console.log("Draft generated. Sending to Discord for approval...");

      const approved = await sendForApproval(email, draft);

      if (approved) {
        const draftId = await createDraft(
          email.from,
          `Re: ${email.subject}`,
          draft,
          email.threadId
        );
        await sendDraft(draftId);
        console.log(`Reply sent to ${email.from}`);
      } else {
        console.log(`Draft rejected for "${email.subject}"`);
      }

      processedMessageIds.add(email.messageId);
    } catch (err) {
      console.error(`Error processing email "${email.subject}":`, err);
    }
  }
}

async function main(): Promise<void> {
  console.log("AugmentAgent starting...");

  await initBot();
  console.log("Discord bot ready.");

  // Initial poll
  await pollAndProcess();

  // Start polling loop
  setInterval(() => {
    pollAndProcess().catch((err) => {
      console.error("Polling error:", err);
    });
  }, POLL_INTERVAL_MS);

  console.log(`Polling every ${POLL_INTERVAL_MS / 1000}s. Press Ctrl+C to stop.`);
}

main().catch((err) => {
  console.error("Fatal error:", err);
  process.exit(1);
});
