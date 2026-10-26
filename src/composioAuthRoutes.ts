import { Router } from "express";
import crypto from "crypto";
import type Composio from "@composio/client";
import type { GoogleAttempt, GoogleAuthFlow } from "./googleAuthRoutes";

const toolkits = { gmail: "gmail", calendar: "googlecalendar", googledrive: "googledrive", slack: "slack" } as const;
type Provider = keyof typeof toolkits;
type Account = Awaited<ReturnType<Composio["connectedAccounts"]["retrieve"]>>;
interface Attempt {
  provider: Provider;
  entityId: string;
  connectionId: string;
  expiresAt: number;
  persisted: boolean;
}
interface Dependencies {
  getClient(): Composio | null;
  getAuthConfig(client: Composio, toolkit: string): Promise<string>;
  getConfig(key: string): string | null | undefined;
  setConfig(key: string, value: string): void;
  getActiveGmailAccounts(): { entityId: string; email?: string | null; label?: string | null }[];
  persist(provider: Provider, connectionId: string, entityId: string, account: Account): Promise<void>;
  /**
   * #1573 — when the operator's own Google OAuth client is configured, a
   * Gmail `start` goes to Google directly (no Composio link) and its
   * attempt is tracked by this flow instead of a Composio connection.
   */
  google?: Pick<GoogleAuthFlow, "configured" | "start" | "status">;
}
class InputError extends Error {}
const attemptKey = (id: string) => `composio_auth_attempt_${id}`;

function googleAttemptView(attempt: GoogleAttempt) {
  const status = attempt.status === "connected" ? "ACTIVE" : attempt.status === "failed" ? "FAILED" : "PENDING";
  return {
    attempt_id: attempt.attempt_id, provider: "gmail", transport: "google", status,
    connected: attempt.status === "connected",
    ...(attempt.email ? { email: attempt.email } : attempt.requested_email ? { email: attempt.requested_email } : {}),
    ...(attempt.entity_id ? { entity_id: attempt.entity_id } : {}),
    ...(attempt.error ? { error: attempt.error } : {}),
  };
}

// Mounted behind the dashboard's authentication middleware. No provider tokens
// or API keys are returned, and no public callback can finalize these attempts.
export function createComposioAuthRouter(deps: Dependencies): Router {
  const router = Router();
  const completions = new Map<string, Promise<void>>();
  router.post("/:operation", async (req, res) => {
    try {
      const operation = req.params.operation;
      if (!["start", "status", "complete"].includes(operation)) {
        res.status(404).json({ error: "Unknown authentication operation" });
        return;
      }
      const body = req.body;
      const keys = operation === "start" ? ["provider", "entity_id", "email"] : ["attempt_id"];
      if (!body || typeof body !== "object" || Array.isArray(body) || Object.keys(body).some(key => !keys.includes(key))) {
        throw new InputError("Invalid authentication arguments");
      }
      if (operation === "start") {
        if (typeof body.provider !== "string" || !Object.hasOwn(toolkits, body.provider)) {
          throw new InputError("provider must be gmail, calendar, googledrive, or slack");
        }
        const provider = body.provider as Provider;
        if (body.entity_id !== undefined && (typeof body.entity_id !== "string" || !body.entity_id.trim() || body.entity_id.length > 256)) {
          throw new InputError("entity_id must be a nonempty string of at most 256 characters");
        }
        let entityId: string = crypto.randomUUID();
        if (provider === "calendar") {
          const accounts = deps.getActiveGmailAccounts();
          const requested = body.entity_id?.trim();
          const entities = [...new Set(accounts.map(account => account.entityId))];
          const selected = requested || (entities.length === 1 ? entities[0] : undefined);
          if (!selected || !entities.includes(selected)) {
            res.status(400).json({ error: "Calendar requires an existing Gmail entity_id", accounts: entities.map(entity_id => {
              const account = accounts.find(item => item.entityId === entity_id)!;
              return { entity_id, ...(account.email ? { email: account.email } : {}), ...(account.label ? { label: account.label } : {}) };
            }) });
            return;
          }
          entityId = selected;
        } else if (body.entity_id !== undefined) {
          throw new InputError("entity_id is only supported for calendar");
        }
        if (body.email !== undefined && (provider !== "gmail" || typeof body.email !== "string"
            || !/^[^\s@]+@[^\s@]+$/.test(body.email.trim()) || body.email.length > 254)) {
          throw new InputError("email must be a mailbox address and is only supported for gmail");
        }
        if (provider === "gmail" && deps.google?.configured()) {
          const started = await deps.google.start(body.email);
          res.status(201).json({
            provider, transport: "google", attempt_id: started.attempt_id, auth_url: started.auth_url,
            expires_at: started.expires_at,
            ...(started.requested_email ? { email: started.requested_email } : {}),
            // Google redirects to the dashboard's registered callback, so the
            // consent must finish in a browser that can reach it.
            note: process.env.AUGMENTAGENT_PUBLIC_URL?.trim()
              ? "Open auth_url on any device that can reach the dashboard's public URL (e.g. one on the same tailnet). The mailbox is saved automatically when the callback lands; poll auth_status until connected."
              : "Open auth_url in a browser on the dashboard host (no public URL is configured, so Google redirects to localhost). The mailbox is saved automatically when the callback lands; poll auth_status until connected.",
          });
          return;
        }
        const client = deps.getClient();
        if (!client) throw new InputError("Composio API key not configured");
        const authConfig = await deps.getAuthConfig(client, toolkits[provider]);
        // Composio hosts the success page. A loopback callback would point at
        // the owner's phone instead of the daemon; status/complete replace it.
        const link = await client.link.create({ user_id: entityId, auth_config_id: authConfig });
        if (!link.connected_account_id || !link.redirect_url || new URL(link.redirect_url).protocol !== "https:") {
          throw new Error("Invalid hosted link response");
        }
        const expiresAt = Math.min(Date.parse(link.expires_at), Date.now() + 24 * 60 * 60 * 1000);
        if (!Number.isFinite(expiresAt) || expiresAt <= Date.now()) throw new Error("Invalid link expiry");
        const attemptId = crypto.randomUUID();
        const attempt: Attempt = { provider, entityId, connectionId: link.connected_account_id, expiresAt, persisted: false };
        deps.setConfig(attemptKey(attemptId), JSON.stringify(attempt));
        res.status(201).json({ provider, entity_id: entityId, connection_id: attempt.connectionId,
          attempt_id: attemptId, auth_url: link.redirect_url, expires_at: new Date(expiresAt).toISOString() });
        return;
      }
      if (typeof body.attempt_id !== "string" || !/^[0-9a-f-]{36}$/.test(body.attempt_id)) {
        throw new InputError("attempt_id must be the identifier returned by start_auth");
      }
      const google = deps.google?.status(body.attempt_id);
      if (google) {
        if (operation === "complete" && google.status !== "connected") {
          res.status(409).json({ error: "Consent is not active", ...googleAttemptView(google) }); return;
        }
        res.json(googleAttemptView(google));
        return;
      }
      const key = attemptKey(body.attempt_id);
      const raw = deps.getConfig(key);
      if (!raw) { res.status(404).json({ error: "Unknown authentication attempt" }); return; }
      const attempt = JSON.parse(raw) as Attempt;
      if (!attempt.persisted && attempt.expiresAt <= Date.now()) {
        res.status(410).json({ error: "Authentication attempt expired; start a new connection" }); return;
      }
      const client = deps.getClient();
      if (!client) throw new InputError("Composio API key not configured");
      const account = await client.connectedAccounts.retrieve(attempt.connectionId);
      if (account.id !== attempt.connectionId || account.user_id !== attempt.entityId || account.toolkit?.slug !== toolkits[attempt.provider]) {
        res.status(409).json({ error: "Connection does not match the authentication attempt" }); return;
      }
      if (operation === "complete") {
        if (account.status !== "ACTIVE") {
          res.status(409).json({ error: "Consent is not active", status: account.status }); return;
        }
        // Serialize retries within the daemon; persistence adapters also dedupe
        // by connection ID so a restart between persistence and receipt is safe.
        let completion = completions.get(key);
        if (!completion) {
          completion = (async () => {
            const latest = JSON.parse(deps.getConfig(key)!) as Attempt;
            if (latest.persisted) return;
            await deps.persist(attempt.provider, attempt.connectionId, attempt.entityId, account);
            deps.setConfig(key, JSON.stringify({ ...latest, persisted: true }));
          })();
          completions.set(key, completion);
        }
        try { await completion; } finally { if (completions.get(key) === completion) completions.delete(key); }
        attempt.persisted = true;
      }
      res.json({ attempt_id: body.attempt_id, provider: attempt.provider, status: account.status,
        connected: account.status === "ACTIVE" && attempt.persisted });
    } catch (error) {
      // SDK errors can include credentials, response bodies, or consent URLs.
      // Keep them out of model-visible errors.
      const input = error instanceof InputError || (error as Error)?.name === "GoogleAuthInputError";
      res.status(input ? 400 : 502).json({
        error: input ? (error as Error).message : "Authentication service failed; retry the operation",
      });
    }
  });
  return router;
}
