import { Composio } from "@composio/client";
import dotenv from "dotenv";
import type { Email } from "./types";

dotenv.config();

export type { Email };

const composio = new Composio({ apiKey: process.env.COMPOSIO_API_KEY });

export async function fetchUnreadEmails(
  senders: string[]
): Promise<Email[]> {
  // TODO: Implement with Composio Gmail integration
  // Will use composio.actions.execute to call GMAIL_FETCH_EMAILS
  throw new Error("Not implemented — wire up Composio Gmail actions");
}

export async function createDraft(
  to: string,
  subject: string,
  body: string,
  threadId?: string
): Promise<string> {
  // TODO: Implement with Composio Gmail integration
  // Will use composio.actions.execute to call GMAIL_CREATE_DRAFT
  throw new Error("Not implemented — wire up Composio Gmail actions");
}

export async function sendDraft(draftId: string): Promise<void> {
  // TODO: Implement with Composio Gmail integration
  // Will use composio.actions.execute to call GMAIL_SEND_DRAFT
  throw new Error("Not implemented — wire up Composio Gmail actions");
}
