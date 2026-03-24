import {
  Agent,
  run,
  tool,
  setTracingDisabled,
} from "@openai/agents";
import { aisdk } from "@openai/agents-extensions/ai-sdk";
import { createGroq } from "@ai-sdk/groq";
import type { Tool } from "@openai/agents";
import { z } from "zod";
import fs from "fs";
import path from "path";
import dotenv from "dotenv";

import { createDraft, sendDraft } from "./gmailService";
import { sendForApproval } from "./discordService";
import { logAction, updateActionStatus, markEmailProcessed } from "./db";
import type { Email } from "./types";

dotenv.config();

setTracingDisabled(true);

const CEREBRAS_MODEL = "gpt-oss-120b";
const GROQ_MODEL = "openai/gpt-oss-120b";
const MAX_RETRIES = 3;
const BASE_DELAY_MS = 1000;

// --- Skill Loading ---

const SKILL_DIR = path.join(process.cwd(), "skills", "email-triage");
const LEARNED_DIR = path.join(SKILL_DIR, "learned");

function loadSkillFile(): string {
  try {
    return fs.readFileSync(path.join(SKILL_DIR, "SKILL.md"), "utf-8");
  } catch {
    return "";
  }
}

function loadLearnedPatterns(): string {
  try {
    if (!fs.existsSync(LEARNED_DIR)) return "";

    const files = fs.readdirSync(LEARNED_DIR).filter((f) => f.endsWith(".json"));
    if (files.length === 0) return "";

    const patterns: string[] = [];
    for (const file of files) {
      try {
        const content = fs.readFileSync(path.join(LEARNED_DIR, file), "utf-8");
        const data = JSON.parse(content);
        if (Array.isArray(data) && data.length > 0) {
          patterns.push(`### ${file.replace(".json", "")}:\n${JSON.stringify(data, null, 2)}`);
        }
      } catch {
        // Skip malformed files
      }
    }

    return patterns.length > 0
      ? `\n## Learned Patterns (from previous cycles)\n${patterns.join("\n\n")}`
      : "";
  } catch {
    return "";
  }
}

function saveLearnedPattern(pattern: {
  type: string;
  pattern: string;
  action: string;
  reason: string;
}): void {
  try {
    if (!fs.existsSync(LEARNED_DIR)) {
      fs.mkdirSync(LEARNED_DIR, { recursive: true });
    }

    const filename = `${pattern.type}-patterns.json`;
    const filepath = path.join(LEARNED_DIR, filename);

    let existing: any[] = [];
    try {
      existing = JSON.parse(fs.readFileSync(filepath, "utf-8"));
    } catch {
      // File doesn't exist yet
    }

    // Deduplicate by pattern string
    if (!existing.some((e) => e.pattern === pattern.pattern)) {
      existing.push({
        pattern: pattern.pattern,
        action: pattern.action,
        reason: pattern.reason,
        learnedAt: new Date().toISOString(),
      });
      fs.writeFileSync(filepath, JSON.stringify(existing, null, 2));
    }
  } catch (err) {
    console.error("Failed to save learned pattern:", err);
  }
}

// --- Providers ---

function createCerebrasModel(model: string) {
  const provider = createGroq({
    apiKey: process.env.CEREBRAS_API_KEY,
    baseURL: "https://api.cerebras.ai/v1",
  });
  return aisdk(provider(model) as any);
}

function createGroqModel(model: string) {
  const provider = createGroq({
    apiKey: process.env.GROQ_API_KEY,
  });
  return aisdk(provider(model) as any);
}

if (!process.env.CEREBRAS_API_KEY && !process.env.GROQ_API_KEY) {
  throw new Error(
    "No LLM provider configured. Set CEREBRAS_API_KEY or GROQ_API_KEY."
  );
}

// --- Compound Tools ---

const gmailTool = (tool as any)({
  name: "gmail",
  description: `Interact with Gmail. Available actions:
- create_draft: Create a reply draft. params: { to: string, subject: string, body: string, threadId?: string, entityId?: string }
- send_draft: Send a draft. params: { draftId: string, entityId?: string }`,
  parameters: z.object({
    action: z.enum(["create_draft", "send_draft"]),
    params: z.record(z.string(), z.unknown()),
  }),
  execute: async (input: { action: string; params: Record<string, unknown> }) => {
    const entityId = input.params.entityId as string | undefined;
    switch (input.action) {
      case "create_draft": {
        const draftId = await createDraft(
          input.params.to as string,
          input.params.subject as string,
          input.params.body as string,
          input.params.threadId as string | undefined,
          entityId
        );
        return JSON.stringify({ draftId });
      }
      case "send_draft": {
        await sendDraft(input.params.draftId as string, entityId);
        return JSON.stringify({ success: true });
      }
      default:
        return JSON.stringify({ error: `Unknown action: ${input.action}` });
    }
  },
}) as Tool;

const notifyTool = (tool as any)({
  name: "notify",
  description: `Send notifications, log actions, and learn patterns. Available actions:
- send_for_approval: Send email + draft to Discord for human approval (opens a thread). User can approve, revise (with feedback), or skip. params: { email: { messageId, threadId, from, subject, body, date }, draft: string }. Returns { approved: boolean, finalDraft: string }. IMPORTANT: use finalDraft (not your original draft) for creating the Gmail draft, as the user may have revised it.
- log_action: Record an action to the database. params: { messageId: string, fromEmail: string, subject: string, originalBody?: string, draftBody?: string, status: "pending"|"approved"|"rejected"|"sent"|"error"|"skipped"|"flagged" }. Returns { actionId: string }
- update_action: Update an existing action's status. params: { actionId: string, status: string, draftBody?: string, errorMessage?: string }
- learn_pattern: Save a pattern for future triage. params: { type: "sender"|"domain"|"subject"|"style", pattern: string, action: "skip"|"flag"|"reply", reason: string }
- mark_processed: Mark an email as processed with triage result. params: { messageId: string, triageResult: "reply"|"skip"|"flag" }`,
  parameters: z.object({
    action: z.enum(["send_for_approval", "log_action", "update_action", "learn_pattern", "mark_processed"]),
    params: z.record(z.string(), z.unknown()),
  }),
  execute: async (input: { action: string; params: Record<string, unknown> }) => {
    switch (input.action) {
      case "send_for_approval": {
        const email = input.params.email as Email;
        const draft = input.params.draft as string;
        const result = await sendForApproval(email, draft);
        return JSON.stringify(result);
      }
      case "log_action": {
        const actionId = logAction({
          messageId: input.params.messageId as string,
          threadId: input.params.threadId as string | undefined,
          fromEmail: input.params.fromEmail as string,
          subject: input.params.subject as string,
          originalBody: input.params.originalBody as string | undefined,
          draftBody: input.params.draftBody as string | undefined,
          status: input.params.status as any,
        });
        return JSON.stringify({ actionId });
      }
      case "update_action": {
        updateActionStatus(
          input.params.actionId as string,
          input.params.status as any,
          {
            draftBody: input.params.draftBody as string | undefined,
            errorMessage: input.params.errorMessage as string | undefined,
          }
        );
        return JSON.stringify({ success: true });
      }
      case "learn_pattern": {
        saveLearnedPattern({
          type: input.params.type as string,
          pattern: input.params.pattern as string,
          action: input.params.action_type as string || input.params.action as string,
          reason: input.params.reason as string,
        });
        return JSON.stringify({ saved: true });
      }
      case "mark_processed": {
        markEmailProcessed(
          input.params.messageId as string,
          input.params.triageResult as string
        );
        return JSON.stringify({ success: true });
      }
      default:
        return JSON.stringify({ error: `Unknown action: ${input.action}` });
    }
  },
}) as Tool;

// --- Agent Instructions (static prefix + skill file, cacheable per session) ---

function buildInstructions(): string {
  const skill = loadSkillFile();
  const learned = loadLearnedPatterns();

  return `You are AugmentAgent, an autonomous email triage and reply agent.

${skill}
${learned}

## Workflow

You receive new emails in context (already fetched). Do NOT call gmail to fetch emails.

For each email provided:
1. TRIAGE: Decide reply/skip/flag using the rules above and learned patterns
2. If SKIP:
   - call notify({ action: "log_action", params: { messageId, fromEmail, subject, status: "skipped" } })
   - call notify({ action: "mark_processed", params: { messageId, triageResult: "skip" } })
3. If FLAG:
   - call notify({ action: "log_action", params: { messageId, fromEmail, subject, status: "flagged" } })
   - call notify({ action: "mark_processed", params: { messageId, triageResult: "flag" } })
4. If REPLY:
   - call notify({ action: "log_action", params: { messageId, fromEmail, subject, originalBody, status: "pending" } })
   - Draft a reply following the Writing Style rules strictly
   - call notify({ action: "update_action", params: { actionId, status: "pending", draftBody: <draft> } })
   - call notify({ action: "send_for_approval", params: { email, draft } })
   - The result includes { approved, finalDraft }. The user may have revised the draft in Discord.
   - If approved: use finalDraft (not your original draft) for create_draft, then send_draft, update status to "sent"
   - If not approved: update status to "rejected"
   - call notify({ action: "mark_processed", params: { messageId, triageResult: "reply" } })
5. After processing all emails, call learn_pattern for any new skip/flag patterns you discovered
6. Report a brief summary: X emails checked, Y skipped, Z drafted, W sent`;
}

const agentTools: Tool[] = [gmailTool, notifyTool];

function createAgent(model: any): Agent {
  return new Agent({
    name: "AugmentAgent",
    instructions: buildInstructions(),
    tools: agentTools,
    model,
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
  const providers: { name: string; model: any; modelId: string }[] = [];

  if (process.env.CEREBRAS_API_KEY) {
    providers.push({
      name: "Cerebras",
      model: createCerebrasModel(CEREBRAS_MODEL),
      modelId: CEREBRAS_MODEL,
    });
  }
  if (process.env.GROQ_API_KEY) {
    providers.push({
      name: "Groq",
      model: createGroqModel(GROQ_MODEL),
      modelId: GROQ_MODEL,
    });
  }

  const errors: string[] = [];

  for (const provider of providers) {
    try {
      console.log(`Trying ${provider.name} (${provider.modelId})...`);
      const agent = createAgent(provider.model);
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

/**
 * Re-draft an email reply based on user feedback.
 * Used by Discord revise flow. Runs a focused agent call with no tools needed.
 */
export async function redraftWithFeedback(
  original: Email,
  previousDraft: string,
  feedback: string
): Promise<string> {
  const skill = loadSkillFile();
  const redraftInstructions = `You are a professional email draft editor. Follow these writing rules strictly:

${skill ? skill.split("## Writing Style")[1]?.split("##")[0] || "" : ""}

Revise the draft below based on the user's feedback. Return ONLY the revised email text, nothing else.`;

  const redraftAgent = new Agent({
    name: "RedraftAgent",
    instructions: redraftInstructions,
    tools: [],
    model: (() => {
      if (process.env.CEREBRAS_API_KEY) return createCerebrasModel(CEREBRAS_MODEL);
      return createGroqModel(GROQ_MODEL);
    })(),
  });

  const input = `Original email from ${original.from}:
Subject: ${original.subject}

${original.body}

---

Previous draft:
${previousDraft}

---

User feedback: "${feedback}"

Write the revised draft now. Only output the email text.`;

  const result = await run(redraftAgent, input);
  return result.finalOutput ?? previousDraft;
}
