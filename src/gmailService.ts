import { Composio } from "@composio/client";
import dotenv from "dotenv";

dotenv.config();

export interface Email {
  messageId: string;
  threadId: string;
  from: string;
  subject: string;
  body: string;
  date: string;
}

const composio = new Composio({ apiKey: process.env.COMPOSIO_API_KEY });

export async function fetchUnreadEmails(
  senders: string[]
): Promise<Email[]> {
  // TODO: Implement with Composio Gmail integration
  // Will use composio.actions.execute to call GMAIL_FETCH_EMAILS
  throw new Error("Not implemented — Phase 1 scaffold");
}

export async function createDraft(
  to: string,
  subject: string,
  body: string,
  threadId?: string
): Promise<string> {
  // TODO: Implement with Composio Gmail integration
  // Will use composio.actions.execute to call GMAIL_CREATE_DRAFT
  throw new Error("Not implemented — Phase 1 scaffold");
}

export async function sendDraft(draftId: string): Promise<void> {
  // TODO: Implement with Composio Gmail integration
  // Will use composio.actions.execute to call GMAIL_SEND_DRAFT
  throw new Error("Not implemented — Phase 1 scaffold");
}
