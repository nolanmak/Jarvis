import {
  Agent,
  run,
  OpenAIChatCompletionsModel,
} from "@openai/agents";
import type { FunctionTool, Tool, RunContext } from "@openai/agents";
import OpenAI from "openai";
import { z } from "zod";
import dotenv from "dotenv";

import { fetchUnreadEmails, createDraft, sendDraft } from "./gmailService";
import { sendForApproval } from "./discordService";
import { logAction, updateActionStatus } from "./db";
import type { Email } from "./types";

dotenv.config();

const MODEL = "gpt-oss-120b";
const MAX_RETRIES = 3;
const BASE_DELAY_MS = 1000;

// --- Providers ---

const cerebrasClient = process.env.CEREBRAS_API_KEY
  ? new OpenAI({
      apiKey: process.env.CEREBRAS_API_KEY,
      baseURL: "https://api.cerebras.ai/v1",
    })
  : null;

const groqClient = process.env.GROQ_API_KEY
  ? new OpenAI({
      apiKey: process.env.GROQ_API_KEY,
      baseURL: "https://api.groq.com/openai/v1",
    })
  : null;

if (!cerebrasClient && !groqClient) {
  throw new Error(
    "No LLM provider configured. Set CEREBRAS_API_KEY or GROQ_API_KEY."
  );
}

// --- Zod schemas for compound tools ---

const gmailSchema = z.object(
  {
    action: z.enum(["fetch_unread", "create_draft", "send_draft"]),
    params: z.record(z.string(), z.unknown()),
  },
);

const notifySchema = z.object(
  {
    action: z.enum(["send_for_approval", "log_action", "update_action"]),
    params: z.record(z.string(), z.unknown()),
  },
);

// --- Compound Tools (2 tools, stable schemas — cacheable) ---

interface GmailInput {
  action: "fetch_unread" | "create_draft" | "send_draft";
  params: Record<string, unknown>;
}

interface NotifyInput {
  action: "send_for_approval" | "log_action" | "update_action";
  params: Record<string, unknown>;
}

const gmailTool: FunctionTool<any, any, any> = {
  type: "function",
  name: "gmail",
  description: `Interact with Gmail. Available actions:
- fetch_unread: Get unread emails. params: { senders: string[] }
- create_draft: Create a reply draft. params: { to: string, subject: string, body: string, threadId?: string }
- send_draft: Send a draft. params: { draftId: string }`,
  parameters: gmailSchema as any,
  strict: true,
  needsApproval: false as any,
  isEnabled: true as any,
  invoke: async (_ctx: RunContext<any>, input: string) => {
    const { action, params } = gmailSchema.parse(JSON.parse(input)) as GmailInput;
    switch (action) {
      case "fetch_unread": {
        const senders = params.senders as string[];
        const emails = await fetchUnreadEmails(senders);
        return JSON.stringify({ emails });
      }
      case "create_draft": {
        const draftId = await createDraft(
          params.to as string,
          params.subject as string,
          params.body as string,
          params.threadId as string | undefined
        );
        return JSON.stringify({ draftId });
      }
      case "send_draft": {
        await sendDraft(params.draftId as string);
        return JSON.stringify({ success: true });
      }
      default:
        return JSON.stringify({ error: `Unknown action: ${action}` });
    }
  },
};

const notifyTool: FunctionTool<any, any, any> = {
  type: "function",
  name: "notify",
  description: `Send notifications and log actions. Available actions:
- send_for_approval: Send email + draft to Discord for human approval. params: { email: { messageId, threadId, from, subject, body, date }, draft: string }. Returns { approved: boolean }
- log_action: Record an action to the database. params: { messageId: string, threadId?: string, fromEmail: string, subject: string, originalBody?: string, draftBody?: string, status: "pending"|"approved"|"rejected"|"sent"|"error" }. Returns { actionId: string }
- update_action: Update an existing action's status. params: { actionId: string, status: string, draftBody?: string, errorMessage?: string }`,
  parameters: notifySchema as any,
  strict: true,
  needsApproval: false as any,
  isEnabled: true as any,
  invoke: async (_ctx: RunContext<any>, input: string) => {
    const { action, params } = notifySchema.parse(JSON.parse(input)) as NotifyInput;
    switch (action) {
      case "send_for_approval": {
        const email = params.email as Email;
        const draft = params.draft as string;
        const approved = await sendForApproval(email, draft);
        return JSON.stringify({ approved });
      }
      case "log_action": {
        const actionId = logAction({
          messageId: params.messageId as string,
          threadId: params.threadId as string | undefined,
          fromEmail: params.fromEmail as string,
          subject: params.subject as string,
          originalBody: params.originalBody as string | undefined,
          draftBody: params.draftBody as string | undefined,
          status: params.status as any,
        });
        return JSON.stringify({ actionId });
      }
      case "update_action": {
        updateActionStatus(
          params.actionId as string,
          params.status as any,
          {
            draftBody: params.draftBody as string | undefined,
            errorMessage: params.errorMessage as string | undefined,
          }
        );
        return JSON.stringify({ success: true });
      }
      default:
        return JSON.stringify({ error: `Unknown action: ${action}` });
    }
  },
};

// --- Agent (static instructions — cacheable prefix) ---

const AGENT_INSTRUCTIONS = `You are AugmentAgent, an autonomous email assistant that checks Gmail and drafts replies for human approval.

When asked to check emails:
1. Call gmail({ action: "fetch_unread", params: { senders: <from context> } })
2. For each new email (not in the processed IDs from context):
   a. Call notify({ action: "log_action", params: { messageId, fromEmail, subject, originalBody, status: "pending" } }) to create a tracking record
   b. Draft a professional, concise reply that matches the original email's tone
   c. Call notify({ action: "update_action", params: { actionId, status: "pending", draftBody: <your draft> } })
   d. Call notify({ action: "send_for_approval", params: { email: <the email object>, draft: <your draft> } })
   e. If approved:
      - Call notify({ action: "update_action", params: { actionId, status: "approved" } })
      - Call gmail({ action: "create_draft", params: { to, subject: "Re: ...", body: <draft>, threadId } })
      - Call gmail({ action: "send_draft", params: { draftId } })
      - Call notify({ action: "update_action", params: { actionId, status: "sent" } })
   f. If rejected:
      - Call notify({ action: "update_action", params: { actionId, status: "rejected" } })
3. If no new emails are found, simply report that.
4. Report a brief summary of actions taken.

Draft guidelines:
- Keep replies brief and to the point
- Match the tone of the original (formal/casual)
- If the email requires action, acknowledge it clearly
- Sign off appropriately`;

const agentTools: Tool[] = [gmailTool, notifyTool];

function createAgent(client: OpenAI): Agent {
  return new Agent({
    name: "AugmentAgent",
    instructions: AGENT_INSTRUCTIONS,
    tools: agentTools,
    model: new OpenAIChatCompletionsModel(client, MODEL),
  });
}

// --- Run with retry + provider fallback ---

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function runWithRetry(
  agent: Agent,
  input: string,
  providerName: string
): Promise<string> {
  for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
    try {
      const result = await run(agent, input);
      return result.finalOutput ?? "";
    } catch (err) {
      const isLast = attempt === MAX_RETRIES - 1;
      const msg = err instanceof Error ? err.message : String(err);

      if (isLast) {
        throw new Error(
          `${providerName} failed after ${MAX_RETRIES} attempts: ${msg}`
        );
      }

      const delay =
        BASE_DELAY_MS * Math.pow(2, attempt) + Math.random() * 100;
      console.warn(
        `${providerName} attempt ${attempt + 1}/${MAX_RETRIES} failed, retrying in ${Math.round(delay)}ms: ${msg}`
      );
      await sleep(delay);
    }
  }

  throw new Error("Unreachable");
}

export async function runAgent(dynamicContext: string): Promise<string> {
  const providers: { name: string; client: OpenAI }[] = [];

  if (cerebrasClient)
    providers.push({ name: "Cerebras", client: cerebrasClient });
  if (groqClient) providers.push({ name: "Groq", client: groqClient });

  const errors: string[] = [];

  for (const provider of providers) {
    try {
      console.log(`Trying ${provider.name} (${MODEL})...`);
      const agent = createAgent(provider.client);
      const result = await runWithRetry(
        agent,
        dynamicContext,
        provider.name
      );
      console.log(`${provider.name} succeeded.`);
      return result;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      errors.push(msg);
      console.warn(`${provider.name} exhausted, falling back...`);
    }
  }

  throw new Error(
    `All LLM providers failed:\n${errors.map((e) => `  - ${e}`).join("\n")}`
  );
}
