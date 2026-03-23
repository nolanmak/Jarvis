import Composio from "@composio/client";
import dotenv from "dotenv";
import type { Email } from "./types";

dotenv.config();

export type { Email };

const composio = new Composio({
  apiKey: process.env.COMPOSIO_API_KEY,
});

export async function fetchUnreadEmails(
  senders: string[]
): Promise<Email[]> {
  const query = senders
    .map((s) => `from:${s}`)
    .join(" OR ");

  const response = await composio.tools.execute("GMAIL_FETCH_EMAILS", {
    arguments: {
      query: `is:unread (${query})`,
      max_results: 20,
    },
  });

  if (response.error) {
    throw new Error(`Composio Gmail fetch failed: ${response.error}`);
  }

  const messages = response.data?.messages as any[] | undefined;
  if (!messages || !Array.isArray(messages)) {
    return [];
  }

  return messages.map((msg: any) => ({
    messageId: msg.id || msg.messageId || "",
    threadId: msg.threadId || "",
    from: msg.from || msg.sender || "",
    subject: msg.subject || "(no subject)",
    body: msg.body || msg.snippet || msg.text || "",
    date: msg.date || msg.internalDate || new Date().toISOString(),
  }));
}

export async function createDraft(
  to: string,
  subject: string,
  body: string,
  threadId?: string
): Promise<string> {
  const args: Record<string, unknown> = {
    to,
    subject,
    body,
  };

  if (threadId) {
    args.thread_id = threadId;
  }

  const response = await composio.tools.execute("GMAIL_CREATE_DRAFT", {
    arguments: args,
  });

  if (response.error) {
    throw new Error(`Composio Gmail create draft failed: ${response.error}`);
  }

  const draftId = response.data?.id || response.data?.draftId;
  if (!draftId) {
    throw new Error("Composio returned no draft ID");
  }

  return String(draftId);
}

export async function sendDraft(draftId: string): Promise<void> {
  const response = await composio.tools.execute("GMAIL_SEND_DRAFT", {
    arguments: {
      draft_id: draftId,
    },
  });

  if (response.error) {
    throw new Error(`Composio Gmail send draft failed: ${response.error}`);
  }
}
