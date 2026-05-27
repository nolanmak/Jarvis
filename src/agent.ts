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
import { getGroupEvents } from "./meetupService";
import { sendForApproval, sendCartForApproval } from "./discordService";
import { logAction, updateActionStatus, markEmailProcessed } from "./db";
import { extractEmailAddress } from "./types";
import type { Email } from "./types";
import { grocery as grocerySidecar } from "./grocerySidecar";
import { kgList, kgRead, kgWrite, kgAppend, recordOrder, type OrderRecord } from "./groceryKg";
import { webFetch as fetchSidecar, type LayerId } from "./fetchSidecar";
import { intercept as runIntercept, interceptAvailable, type InterceptParams } from "./interceptTool";

dotenv.config();

setTracingDisabled(true);

const CEREBRAS_MODEL = "gpt-oss-120b";
const GROQ_MODEL = "openai/gpt-oss-120b";
const MAX_RETRIES = 3;
const BASE_DELAY_MS = 1000;

// --- Skill Loading ---

const SKILLS_ROOT = path.join(process.cwd(), "skills");

function skillDir(name: string): string {
  return path.join(SKILLS_ROOT, name);
}

function loadSkillFile(name: string = "email-triage"): string {
  try {
    return fs.readFileSync(path.join(skillDir(name), "SKILL.md"), "utf-8");
  } catch {
    return "";
  }
}

function loadLearnedPatterns(name: string = "email-triage"): string {
  try {
    const learnedDir = path.join(skillDir(name), "learned");
    if (!fs.existsSync(learnedDir)) return "";

    const files = fs.readdirSync(learnedDir).filter((f) => f.endsWith(".json"));
    if (files.length === 0) return "";

    const patterns: string[] = [];
    for (const file of files) {
      try {
        const content = fs.readFileSync(path.join(learnedDir, file), "utf-8");
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

function saveLearnedPattern(
  pattern: { type: string; pattern: string; action: string; reason: string },
  skillName: string = "email-triage",
): void {
  try {
    const learnedDir = path.join(skillDir(skillName), "learned");
    if (!fs.existsSync(learnedDir)) {
      fs.mkdirSync(learnedDir, { recursive: true });
    }

    const filename = `${pattern.type}-patterns.json`;
    const filepath = path.join(learnedDir, filename);

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
- send_for_approval: Send email + draft to Discord for human approval (opens a thread). User can approve, revise (with feedback), skip, or change the recipient. params: { actionId: string, email: { messageId, threadId, from, subject, body, date }, draft: string, recipientEmail: string }. Returns { approved: boolean, finalDraft: string, finalRecipient: string }. IMPORTANT: use finalDraft AND finalRecipient (not the originals) for creating the Gmail draft — the user may have revised either.
- log_action: Record an action to the database. params: { messageId: string, fromEmail: string, recipientEmail?: string, subject: string, originalBody?: string, draftBody?: string, status: "pending"|"approved"|"rejected"|"sent"|"error"|"skipped"|"flagged" }. Returns { actionId: string }
- update_action: Update an existing action's status. params: { actionId: string, status: string, draftBody?: string, recipientEmail?: string, errorMessage?: string }
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
        // #36: recipientEmail flows through so the Discord card can display a
        // "To:" line AND offer a Change-To override. Falls back to the parsed
        // sender if the agent forgot to pass it (older prompts).
        const initialRecipient =
          (input.params.recipientEmail as string | undefined) ||
          extractEmailAddress(email.from);
        const actionId = input.params.actionId as string | undefined;
        const result = await sendForApproval(email, draft, actionId, initialRecipient);
        return JSON.stringify(result);
      }
      case "log_action": {
        // #36: default recipientEmail to the parsed sender so the action row
        // always carries the address that WILL be used at send time. The UI
        // and the agent can both edit it later via update_action.
        const fromEmail = input.params.fromEmail as string;
        const recipientEmail =
          (input.params.recipientEmail as string | undefined) ||
          extractEmailAddress(fromEmail);
        const actionId = logAction({
          messageId: input.params.messageId as string,
          threadId: input.params.threadId as string | undefined,
          fromEmail,
          recipientEmail,
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
            recipientEmail: input.params.recipientEmail as string | undefined,
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

const meetupTool = (tool as any)({
  name: "meetup_events",
  description: `List a Meetup group's events via Meetup's API (no auth needed for public groups). Available actions:
- list_events: Fetch a group's events. params: { urlname: string (group slug from meetup.com/<urlname>/, e.g. "code-coffee-philly"), kind?: "upcoming" | "past" (default "upcoming"), limit?: number (default all) }
Returns JSON: { totalCount, count, events: [{ id, title, url, status, dateTime, endTime, isOnline, eventType, going, venue, recurrence }] }`,
  parameters: z.object({
    action: z.enum(["list_events"]),
    params: z.record(z.string(), z.unknown()),
  }),
  execute: async (input: { action: string; params: Record<string, unknown> }) => {
    switch (input.action) {
      case "list_events": {
        const urlname = input.params.urlname as string;
        if (!urlname) return JSON.stringify({ error: "urlname is required" });
        const kind = input.params.kind === "past" ? "past" : "upcoming";
        const rawLimit = Number(input.params.limit);
        const limit = Number.isFinite(rawLimit) && rawLimit > 0 ? rawLimit : undefined;
        try {
          const { totalCount, events } = await getGroupEvents(urlname, { kind, limit });
          return JSON.stringify({ totalCount, count: events.length, events });
        } catch (err: any) {
          return JSON.stringify({ error: err?.message ?? String(err) });
        }
      }
      default:
        return JSON.stringify({ error: `Unknown action: ${input.action}` });
    }
  },
}) as Tool;

const groceryTool = (tool as any)({
  name: "grocery",
  description: `Order groceries from the configured provider (default: Giant Food Stores) and read/write the wiki/groceries/ knowledge graph.

The tool is the hands, you are the brain. Search returns options; you decide which products to add. The flow ends at a Discord approval card — checkout itself is finished by the user in the browser.

Available actions:
- session_check: Verify the grocery store session. No params. Returns { authenticated, userId?, storeId?, firstName? }.
- login: Log in to the grocery store. params: { email?, password? } (both default to env vars). If OTP required, returns { status: "otp_sent", channel, maskedValue }. If success, returns { status: "success", userId }.
- verify_otp: Complete OTP login. params: { code: string, channel?: string }. Returns { userId }.
- search: Search products. params: { query: string, limit?: number }. Returns { query, products: [{ prodId, name, brand, price, regularPrice, size, inStock, onSale }] }.
- search_batch: Search multiple queries at once. params: { queries: string[], limit_per_query?: number }. Returns { results: [...] }.
- products_by_id: Look up known products by id (fallback when search misses a known staple). params: { prodIds: number[] }.
- cart_view: Show current cart. No params. Returns { cartId, items, subtotal, tax, total, storeId }.
- cart_add: Add items by product id. params: { items: [{ productId, quantity }] }. Returns { added, errors }.
- cart_remove: Remove an item by product id. params: { productId }.
- submit_for_approval: Post the cart summary to Discord and wait for human approve/skip. params: { title: string, body_md: string }. Returns { approved: boolean, feedback: string }. ALWAYS use this before treating the order as final.
- kg_list: List all pages under wiki/groceries/. No params. Returns { pages: string[] }.
- kg_read: Read a page from wiki/groceries/. params: { page: string } (relative path under wiki/groceries/, e.g. "staples.md", "orders/2026-05-21.md"). Returns { page, content }.
- kg_write: Overwrite a page in wiki/groceries/. params: { page: string, content: string }.
- kg_append: Append to a page in wiki/groceries/. params: { page: string, content: string }.
- record_order: Append a structured order record to wiki/groceries/orders/<date>.md. params: { date?: string (YYYY-MM-DD, defaults to today), order: { items_ordered: [{ name, qty, brand?, prodId?, price? }], items_oos?, substitutions?, total?, feedback?, approved? } }. Returns { page }.
- schedule_set: Create or replace a grocery-order schedule via systemd user timers. params: { kind: "recurring" | "oneshot", oncalendar: string (systemd OnCalendar=, e.g. "Sun *-*-* 10:00:00" or "2026-05-30 09:00:00"), label?: string (required for oneshot, matches ^[a-z0-9-]{1,32}$) }. Recurring is a single slot — calling again replaces the previous recurring timer. Returns { ok: true, unit, next, kind, oncalendar }.
- schedule_list: List scheduled grocery-order timers. No params. Returns [{ name, next, last, kind }].
- schedule_clear: Cancel a grocery schedule. params: { name?: string } (omit name to clear all). Returns { ok: true, cleared: [unit, ...] }.`,
  parameters: z.object({
    action: z.enum([
      "session_check",
      "login",
      "verify_otp",
      "search",
      "search_batch",
      "products_by_id",
      "cart_view",
      "cart_add",
      "cart_remove",
      "submit_for_approval",
      "kg_list",
      "kg_read",
      "kg_write",
      "kg_append",
      "record_order",
      "schedule_set",
      "schedule_list",
      "schedule_clear",
    ]),
    params: z.record(z.string(), z.unknown()).nullable().optional(),
  }),
  execute: async (input: { action: string; params?: Record<string, unknown> | null }) => {
    const params = input.params ?? {};
    try {
      switch (input.action) {
        case "session_check":
          return JSON.stringify(await grocerySidecar.sessionCheck());
        case "login":
          return JSON.stringify(await grocerySidecar.login(params.email as string | undefined, params.password as string | undefined));
        case "verify_otp": {
          const code = params.code as string | undefined;
          if (!code) return JSON.stringify({ error: "verify_otp requires { code }" });
          return JSON.stringify(await grocerySidecar.verifyOtp(code, params.channel as string | undefined));
        }
        case "search": {
          const query = params.query as string | undefined;
          if (!query) return JSON.stringify({ error: "search requires { query }" });
          const limit = Number(params.limit);
          return JSON.stringify(await grocerySidecar.search(query, Number.isFinite(limit) ? limit : undefined));
        }
        case "search_batch": {
          const queries = params.queries;
          if (!Array.isArray(queries)) return JSON.stringify({ error: "search_batch requires { queries: string[] }" });
          const lpq = Number(params.limit_per_query);
          return JSON.stringify(await grocerySidecar.searchBatch(queries.map(String), Number.isFinite(lpq) ? lpq : undefined));
        }
        case "products_by_id": {
          const prodIds = params.prodIds;
          if (!Array.isArray(prodIds)) return JSON.stringify({ error: "products_by_id requires { prodIds: number[] }" });
          return JSON.stringify(await grocerySidecar.productsById(prodIds as Array<string | number>));
        }
        case "cart_view":
          return JSON.stringify(await grocerySidecar.cartView());
        case "cart_add": {
          const items = params.items;
          if (!Array.isArray(items)) return JSON.stringify({ error: "cart_add requires { items: [{ productId, quantity }] }" });
          return JSON.stringify(await grocerySidecar.cartAdd(items as any));
        }
        case "cart_remove": {
          const productId = params.productId;
          if (productId == null) return JSON.stringify({ error: "cart_remove requires { productId }" });
          return JSON.stringify(await grocerySidecar.cartRemove(productId as any));
        }
        case "submit_for_approval": {
          const title = (params.title as string) ?? "Grocery cart for review";
          const body = (params.body_md as string) ?? "(empty cart)";
          const result = await sendCartForApproval(title, body);
          return JSON.stringify(result);
        }
        case "kg_list":
          return JSON.stringify({ pages: kgList() });
        case "kg_read": {
          const page = params.page as string | undefined;
          if (!page) return JSON.stringify({ error: "kg_read requires { page }" });
          return JSON.stringify({ page, content: kgRead(page) });
        }
        case "kg_write": {
          const page = params.page as string | undefined;
          const content = params.content as string | undefined;
          if (!page || content == null) return JSON.stringify({ error: "kg_write requires { page, content }" });
          kgWrite(page, content);
          return JSON.stringify({ success: true, page });
        }
        case "kg_append": {
          const page = params.page as string | undefined;
          const content = params.content as string | undefined;
          if (!page || content == null) return JSON.stringify({ error: "kg_append requires { page, content }" });
          kgAppend(page, content);
          return JSON.stringify({ success: true, page });
        }
        case "record_order": {
          const date = (params.date as string) || new Date().toISOString().slice(0, 10);
          const order = params.order as OrderRecord | undefined;
          if (!order || !Array.isArray(order.items_ordered)) {
            return JSON.stringify({ error: "record_order requires { order: { items_ordered: [...] } }" });
          }
          const page = recordOrder(date, order);
          return JSON.stringify({ success: true, page });
        }
        case "schedule_set": {
          const kind = params.kind as string | undefined;
          const oncalendar = params.oncalendar as string | undefined;
          const label = params.label as string | undefined;
          if (kind !== "recurring" && kind !== "oneshot") {
            return JSON.stringify({ error: "schedule_set requires { kind: 'recurring' | 'oneshot' }" });
          }
          if (!oncalendar || typeof oncalendar !== "string") {
            return JSON.stringify({ error: "schedule_set requires { oncalendar: string }" });
          }
          if (kind === "oneshot" && (!label || !/^[a-z0-9-]{1,32}$/.test(label))) {
            return JSON.stringify({ error: "schedule_set kind=oneshot requires { label: ^[a-z0-9-]{1,32}$ }" });
          }
          return JSON.stringify(await grocerySidecar.scheduleSet(kind as "recurring" | "oneshot", oncalendar, label));
        }
        case "schedule_list":
          return JSON.stringify(await grocerySidecar.scheduleList());
        case "schedule_clear": {
          const name = params.name as string | undefined;
          return JSON.stringify(await grocerySidecar.scheduleClear(name));
        }
        default:
          return JSON.stringify({ error: `Unknown action: ${input.action}` });
      }
    } catch (err: any) {
      return JSON.stringify({ error: err?.message ?? String(err), kind: err?.kind });
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
   - Determine recipientEmail: the bare email address parsed out of the email's "from" header (strip any display name and angle brackets). The operator can override this on the approval card.
   - call notify({ action: "log_action", params: { messageId, fromEmail, recipientEmail, subject, originalBody, status: "pending" } })
   - Draft a reply following the Writing Style rules strictly
   - call notify({ action: "update_action", params: { actionId, status: "pending", draftBody: <draft> } })
   - call notify({ action: "send_for_approval", params: { actionId, email, draft, recipientEmail } })
   - The result includes { approved, finalDraft, finalRecipient }. The user may have revised the draft AND/OR changed the recipient in Discord.
   - If approved: call gmail({ action: "create_draft", params: { to: finalRecipient, subject, body: finalDraft, threadId, entityId } }), then send_draft, then update_action to "sent"
   - If not approved: update_action to "rejected"
   - call notify({ action: "mark_processed", params: { messageId, triageResult: "reply" } })
5. After processing all emails, call learn_pattern for any new skip/flag patterns you discovered
6. Report a brief summary: X emails checked, Y skipped, Z drafted, W sent`;
}

const webFetchTool = (tool as any)({
  name: "web_fetch",
  description: `Fetch a URL and return rendered markdown. Tries strategies in order:
1. plain HTTPS (fastest, free)
2. headless Chromium render (handles JS-rendered SPAs)
3. Firecrawl API (if FIRECRAWL_API_KEY set) — fallback for sites where local render fails
4. Bright Data Unlocker (if BRIGHTDATA_API_KEY + BRIGHTDATA_ZONE set) — final fallback for anti-bot / captcha sites

Layers without configured API keys are skipped automatically. Use this for any URL fetch — do NOT make raw HTTP calls.

params:
- url (required): the URL to fetch
- layers (optional): subset of ["http","render","firecrawl","brightdata"] to restrict to specific layers
- force_render (optional): true to skip plain HTTP and start at the render layer
- min_quality_chars (optional, default 400): if returned markdown is shorter, the next layer is tried
- timeout_ms (optional, default 90000): per-layer timeout
- include_html (optional): include raw HTML in response (default: no)

returns: { url, final_url, status, title, markdown, layer_used, attempts: [...], elapsed_ms }`,
  parameters: z.object({
    url: z.string(),
    layers: z.array(z.enum(["http", "render", "firecrawl", "brightdata"])).nullable().optional(),
    force_render: z.boolean().nullable().optional(),
    min_quality_chars: z.number().int().positive().nullable().optional(),
    timeout_ms: z.number().int().positive().nullable().optional(),
    include_html: z.boolean().nullable().optional(),
  }),
  execute: async (input: {
    url: string;
    layers?: LayerId[];
    force_render?: boolean;
    min_quality_chars?: number;
    timeout_ms?: number;
    include_html?: boolean;
  }) => {
    try {
      const layers: LayerId[] | undefined = input.force_render
        ? (input.layers ?? ["render", "firecrawl", "brightdata"])
        : input.layers;
      const r = await fetchSidecar.fetch({
        url: input.url,
        layers,
        min_quality_chars: input.min_quality_chars,
        timeout_ms: input.timeout_ms,
      });
      return JSON.stringify({
        url: r.url,
        final_url: r.final_url,
        status: r.status,
        title: r.title,
        markdown: r.markdown,
        ...(input.include_html ? { html: r.html } : {}),
        layer_used: r.layer_used,
        attempts: r.attempts,
        elapsed_ms: r.elapsed_ms,
      });
    } catch (err: any) {
      return JSON.stringify({ error: err?.message ?? String(err), kind: err?.kind });
    }
  },
}) as Tool;

const interceptTool = (tool as any)({
  name: "intercept",
  description: `Read-only query of the local Claude Intercept MITM proxy (port 7777). Inspects HTTP/S traffic the operator has ALREADY captured via the global /intercept skill. Useful for:
- API discovery on sites with no public docs (analyze a real browser session the operator already captured)
- Auth extraction (pull Bearer tokens / cookies / API keys from a capture the operator collected)
- Debugging failed scrapes (compare what curl sees vs what a real browser sent)

This is a local-only, read-only tool. It cannot start or stop the proxy and cannot install / trust the CA cert — those are dangerous machine-wide operations and stay with the human operator's /intercept skill. Returns proxy state or captured traffic; never sends data to remote services.

Actions:
- status: is the proxy running, how many requests captured
- export: dump captured traffic. params: { mode: "api-docs" | "auth" | "summary" | "full", host?: string, limit?: number }
- clear: delete the local capture buffer (does NOT touch the proxy itself)

If the proxy is not installed, the tool reports it cleanly — do not retry. If status reports the proxy is OFF, ask the operator to enable it via the /intercept skill rather than trying to start it yourself.`,
  parameters: z.object({
    action: z.enum(["status", "export", "clear"]),
    mode: z.enum(["api-docs", "auth", "summary", "full"]).nullable().optional(),
    host: z.string().nullable().optional(),
    limit: z.number().int().positive().nullable().optional(),
  }),
  execute: async (input: InterceptParams) => {
    try {
      if (!interceptAvailable() && input.action !== "status") {
        return JSON.stringify({ ok: false, error: "Claude Intercept not installed on this machine" });
      }
      const r = await runIntercept(input);
      return JSON.stringify(r);
    } catch (err: any) {
      return JSON.stringify({ ok: false, error: err?.message ?? String(err) });
    }
  },
}) as Tool;

const agentTools: Tool[] = [gmailTool, notifyTool, meetupTool, groceryTool, webFetchTool, interceptTool];

function createAgent(model: any, instructions: string = buildInstructions()): Agent {
  return new Agent({
    name: "AugmentAgent",
    instructions,
    tools: agentTools,
    model,
  });
}

// Ad-hoc query mode: same tools, but the agent answers a one-off request
// instead of running the email-triage workflow. Used by POST /api/ask.
function buildQueryInstructions(): string {
  const grocerySkill = loadSkillFile("grocery");
  const groceryLearned = loadLearnedPatterns("grocery");

  return `You are AugmentAgent answering a one-off request from the operator. Use the available tools to answer, then reply with the answer only — no triage, no email processing, no Discord approval (unless the grocery workflow specifically requires it).

## Meetup events
Use the meetup_events tool for any request about Meetup events. Group name → urlname mapping:
- "C&C", "Code & Coffee", "Coffee & Code", "code coffee", "the meetup" → urlname "code-coffee-philly"
If the user names another group, use their slug from meetup.com/<urlname>/.

When asked for events (e.g. "C&C events on meetup", "upcoming Code & Coffee meetups"):
1. call meetup_events({ action: "list_events", params: { urlname: "code-coffee-philly", kind: "upcoming", limit: 10 } }) — default limit 10. Use the user's number if they ask for a specific count; omit limit only if they explicitly ask for "all" events. Use kind "past" only if the user explicitly asks for past events.
2. Format the result as a clean Markdown list, one event per item, sorted by time (earliest first):
   - **<title>** — <day, date, start time in a human-readable form>
   - <location: venue name + city, or "Online" if isOnline> · <url>
3. Start with a one-line header like "Upcoming Code & Coffee events (<count> of <totalCount>):". If there are no events, say so plainly. If the tool returns an { error }, report it briefly (a stale-hash error means the Meetup API hash needs refreshing via /intercept).
Keep it concise — title, time, location, link. No descriptions unless asked.

## Groceries
Use the grocery tool for any request about ordering groceries, the staples list, pantry state, preferences, dislikes, or past orders. Knowledge lives in wiki/groceries/ — always read it before acting and write back what you learn.

${grocerySkill}
${groceryLearned}

## Web fetching
For ANY URL the operator asks you to read, summarize, or pull data from, use the web_fetch tool — never assume a URL's contents or make raw HTTP calls. It tries plain HTTPS first (free), escalates to a headless-Chromium render for JS-rendered SPAs, then to Firecrawl / Bright Data if API keys are configured. If the first response looks like an empty SPA shell or is unexpectedly short, retry with force_render: true. The returned markdown is what you should reason over; cite final_url when surfacing links.

## Intercept (optional, debugging only)
If a fetch fails repeatedly or you need to discover an undocumented API / extract auth from a captured browser session, use the intercept tool to query the local MITM proxy (status, export with mode "api-docs" | "auth" | "summary" | "full"). Read-only inspection: you can read captures the operator has already collected but cannot start/stop the proxy or install the CA cert — if the proxy is off, ask the operator to enable it via the /intercept skill. If the proxy isn't installed the tool reports it cleanly; don't retry.`;
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

// Run `input` through each configured provider in turn (Cerebras → Groq),
// retrying within a provider before falling back. `instructions` selects the
// agent's mode: email-triage (default) or ad-hoc query.
async function runWithProviders(
  input: string,
  instructions: string
): Promise<string> {
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
      const agent = createAgent(provider.model, instructions);
      const result = await runWithRetry(agent, input, provider.name);
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

export async function runAgent(dynamicContext: string): Promise<string> {
  return runWithProviders(dynamicContext, buildInstructions());
}

/**
 * Answer a one-off operator request (e.g. "C&C events on meetup") using the
 * agent's tools, bypassing the email-triage workflow. Backs POST /api/ask.
 */
export async function runAgentQuery(question: string): Promise<string> {
  return runWithProviders(question, buildQueryInstructions());
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
