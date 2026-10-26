// Agent- and CLI-initiated Gmail connects on the operator's own Google OAuth
// client (#1573). One attempt = one consent: `start` mints the Google consent
// URL with a random `state`, the browser callback (`/oauth/google/callback`)
// finishes it, and `status` tells the caller (dashboard, CLI, or the Discord
// agent through the Composio auth MCP tool) whether the mailbox landed.
//
// Unlike Composio's hosted link, Google redirects to THIS dashboard's
// registered callback (localhost unless the operator registered a public
// hostname), so the consent has to finish in a browser that can reach the
// dashboard. The attempt record is what lets a caller on another device
// still learn the outcome.

export type GoogleAttemptStatus = "pending" | "connected" | "failed";

export interface GoogleAttempt {
  attempt_id: string;
  createdAt: number;
  expiresAt: number;
  status: GoogleAttemptStatus;
  email?: string;
  entity_id?: string;
  error?: string;
}

export interface GoogleAuthDeps {
  configured(): boolean;
  redirectUri(): string;
  getConfig(key: string): string | null | undefined;
  setConfig(key: string, value: string): void;
  deleteConfig(key: string): void;
  randomId(): string;
  /** Run the agent CLI; resolves with exit code + captured output. */
  runCli(
    args: string[],
    stdin?: string,
    timeoutMs?: number,
  ): Promise<{ code: number | null; stdout: string; stderr: string }>;
  now?(): number;
}

export const GOOGLE_ATTEMPT_TTL_MS = 10 * 60 * 1000;
export const attemptKey = (id: string) => `google_oauth_attempt_${id}`;

export class GoogleAuthInputError extends Error {}

export function createGoogleAuthFlow(deps: GoogleAuthDeps) {
  const now = () => (deps.now ? deps.now() : Date.now());

  function read(attemptId: string): GoogleAttempt | null {
    const raw = deps.getConfig(attemptKey(attemptId));
    if (!raw) return null;
    try {
      return JSON.parse(raw) as GoogleAttempt;
    } catch {
      return null;
    }
  }

  function write(attempt: GoogleAttempt): void {
    deps.setConfig(attemptKey(attempt.attempt_id), JSON.stringify(attempt));
  }

  return {
    configured: deps.configured,

    /** Mint a consent URL and record a pending attempt keyed by its state. */
    async start(): Promise<{ attempt_id: string; auth_url: string; expires_at: string }> {
      if (!deps.configured()) {
        throw new GoogleAuthInputError(
          "GOOGLE_OAUTH_CLIENT_ID / GOOGLE_OAUTH_CLIENT_SECRET are not set; see .env.example",
        );
      }
      const attemptId = deps.randomId();
      const out = await deps.runCli([
        "google",
        "auth-url",
        "--redirect-uri",
        deps.redirectUri(),
        "--state",
        attemptId,
      ]);
      const url = out.stdout.trim().split("\n").pop() ?? "";
      if (out.code !== 0 || !url.startsWith("https://accounts.google.com/")) {
        throw new Error(out.stderr.trim() || "could not build the Google consent URL");
      }
      const createdAt = now();
      const attempt: GoogleAttempt = {
        attempt_id: attemptId,
        createdAt,
        expiresAt: createdAt + GOOGLE_ATTEMPT_TTL_MS,
        status: "pending",
      };
      write(attempt);
      return {
        attempt_id: attemptId,
        auth_url: url,
        expires_at: new Date(attempt.expiresAt).toISOString(),
      };
    },

    /** The attempt as last recorded; a pending one past its TTL reads as failed. */
    status(attemptId: string): GoogleAttempt | null {
      const attempt = read(attemptId);
      if (!attempt) return null;
      if (attempt.status === "pending" && attempt.expiresAt <= now()) {
        return { ...attempt, status: "failed", error: "consent not completed in time" };
      }
      return attempt;
    },

    /**
     * Browser callback. Verifies the state against a pending attempt, hands
     * the code to the CLI (over stdin) and records the outcome.
     */
    async finish(
      state: string,
      code: string,
      providerError: string,
    ): Promise<{ ok: boolean; message: string; attempt?: GoogleAttempt }> {
      const attempt = state ? read(state) : null;
      if (!attempt || attempt.status !== "pending") {
        return {
          ok: false,
          message: "This connect attempt is unknown or already finished. Start again.",
        };
      }
      if (attempt.expiresAt <= now()) {
        write({ ...attempt, status: "failed", error: "consent not completed in time" });
        return { ok: false, message: "This connect attempt expired. Start again." };
      }
      if (providerError || !code) {
        const failed: GoogleAttempt = {
          ...attempt,
          status: "failed",
          error: providerError || "no authorization code returned",
        };
        write(failed);
        return { ok: false, message: `Google did not grant access (${failed.error}).`, attempt: failed };
      }
      const out = await deps.runCli(
        ["google", "connect", "--redirect-uri", deps.redirectUri(), "--json"],
        code,
        120_000,
      );
      if (out.code !== 0) {
        // The CLI's stderr can quote Google's error body; keep it out of the
        // attempt record the agent reads back and log it server-side instead.
        console.error("[oauth] google connect failed:", out.stderr.trim());
        const failed: GoogleAttempt = {
          ...attempt,
          status: "failed",
          error: "the code exchange failed; see the dashboard log",
        };
        write(failed);
        return { ok: false, message: out.stderr.trim() || "the code exchange failed", attempt: failed };
      }
      let email: string | undefined;
      let entityId: string | undefined;
      try {
        const parsed = JSON.parse(out.stdout.trim()) as { email?: string; entity_id?: string };
        email = parsed.email;
        entityId = parsed.entity_id;
      } catch {
        /* the connect succeeded; identity is still readable from gmail_accounts */
      }
      const done: GoogleAttempt = { ...attempt, status: "connected", email, entity_id: entityId };
      write(done);
      return { ok: true, message: `connected ${email ?? "mailbox"}`, attempt: done };
    },
  };
}

export type GoogleAuthFlow = ReturnType<typeof createGoogleAuthFlow>;
