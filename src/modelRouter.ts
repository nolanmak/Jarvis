import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";

export type Provider = "claude" | "codex";
export type RouteMode = "direct" | "auto" | Provider | "qwen" | "glm";
export interface RouterConfig {
  version: 1;
  mode: RouteMode;
  base_url: string;
  api_key: string;
  admin_password?: string;
  models: Record<Provider, { quality: string; fast: string }>;
}
export function validateConfig(value: unknown): RouterConfig {
  const c = value as RouterConfig;
  if (
    !c ||
    c.version !== 1 ||
    !["direct", "auto", "claude", "codex", "qwen", "glm"].includes(c.mode)
  )
    throw new Error("Invalid model route");
  let url: URL;
  try {
    url = new URL(c.base_url);
  } catch {
    throw new Error("Invalid router endpoint");
  }
  const local =
    url.protocol === "http:" &&
    ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  const remotePort = url.port || (url.protocol === "https:" ? "443" : "");
  const authority = `${url.hostname}:${remotePort}`;
  const allowed = (process.env.AUGMENTAGENT_MODEL_ROUTER_ALLOWED_HOSTS || "")
    .split(",")
    .some((entry) => entry.trim().toLowerCase() === authority.toLowerCase());
  const remote = !!remotePort && allowed && url.protocol === "https:";
  if (
    !(local || remote) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    url.pathname !== "/v1"
  )
    throw new Error("Router endpoint must be loopback or an explicitly allowed remote /v1 endpoint");
  if (
    typeof c.api_key !== "string" ||
    !c.api_key.trim() ||
    /[\x00-\x1f\x7f]/.test(c.api_key)
  )
    throw new Error("Missing or invalid router API key");
  for (const [provider, prefix] of [
    ["claude", "cc/"],
    ["codex", "cx/"],
  ] as const) {
    for (const tier of ["quality", "fast"] as const) {
      const model = c.models?.[provider]?.[tier];
      if (
        typeof model !== "string" ||
        !model.startsWith(prefix) ||
        model.length <= prefix.length ||
        model.length > 160 ||
        !/^[a-zA-Z0-9_./:-]+$/.test(model)
      )
        throw new Error(
          "Models must stay within their provider: cc/ for Claude, cx/ for Codex",
        );
    }
  }
  return c;
}
export function publicConfig(c: RouterConfig | null) {
  if (!c) return null;
  return {
    version: c.version,
    mode: c.mode,
    base_url: c.base_url,
    models: c.models,
    has_key: !!c.api_key,
  };
}
export class ModelRouterStore {
  constructor(
    public readonly file = process.env.AUGMENTAGENT_MODEL_ROUTER_CONFIG ||
      path.join(
        process.env.XDG_CONFIG_HOME || path.join(os.homedir(), ".config"),
        "augmentagent",
        "model-router.json",
      ),
  ) {}
  read(): RouterConfig | null {
    let raw: string;
    try {
      raw = fs.readFileSync(this.file, "utf8");
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code === "ENOENT") return null;
      throw new Error("Cannot read model router configuration");
    }
    try {
      return validateConfig(JSON.parse(raw));
    } catch {
      throw new Error("Invalid model router configuration");
    }
  }
  write(value: RouterConfig): void {
    const c = validateConfig(value);
    const tmp = `${this.file}.${crypto.randomUUID()}.tmp`;
    try {
      fs.mkdirSync(path.dirname(this.file), { recursive: true, mode: 0o700 });
      try {
        const fd = fs.openSync(tmp, "wx", 0o600);
        try {
          fs.writeFileSync(fd, JSON.stringify(c, null, 2) + "\n");
          fs.fsyncSync(fd);
        } finally {
          fs.closeSync(fd);
        }
        fs.renameSync(tmp, this.file);
      } finally {
        fs.rmSync(tmp, { force: true });
      }
    } catch (error) {
      console.error("[model-router] configuration write failed", error);
      throw new Error("Cannot save model router configuration");
    }
  }
}
export interface Account {
  id: string;
  provider: Provider;
  name: string;
  active: boolean;
  priority: number;
  status: string;
}
export class RouterClient {
  constructor(
    private readonly config: RouterConfig,
    private readonly request: typeof fetch = fetch,
  ) {
    validateConfig(config);
  }
  async api(endpoint: string, method = "GET", body?: unknown): Promise<any> {
    if (!this.config.admin_password)
      throw new Error(
        "9Router management is not configured. Run the installer.",
      );
    const base = this.config.base_url.slice(0, -3);
    const login = await this.request(`${base}/api/auth/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ password: this.config.admin_password }),
      signal: AbortSignal.timeout(8000),
      redirect: "error",
    });
    const cookie = login.headers
      .getSetCookie()
      .find((c) => c.startsWith("auth_token="))
      ?.split(";")[0];
    if (!login.ok || !cookie)
      throw new Error("9Router management login failed");
    const response = await this.request(`${base}${endpoint}`, {
      method,
      headers: { Cookie: cookie, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(30000),
      redirect: "error",
    });
    if (!response.ok)
      throw new Error(`9Router request failed (${response.status})`);
    return response.json();
  }
  async accounts(): Promise<Account[]> {
    const data = await this.api("/api/providers");
    if (!Array.isArray(data.connections))
      throw new Error("Invalid account response from 9Router");
    return data.connections
      .filter((c: any) => ["claude", "codex"].includes(c.provider))
      .map((c: any) => ({
        id: String(c.id),
        provider: c.provider,
        name: String(c.name || c.email || c.displayName || "Connected account"),
        active: c.isActive === true,
        priority: Number(c.priority || 1),
        status: String(c.testStatus || "unknown"),
      }));
  }
  async updateAccount(
    id: string,
    active: boolean,
    priority: number,
  ): Promise<void> {
    if (!Number.isInteger(priority) || priority < 1 || priority > 999)
      throw new Error("Priority must be between 1 and 999");
    if (!(await this.accounts()).some((a) => a.id === id))
      throw new Error("Account not found");
    await this.api(`/api/providers/${encodeURIComponent(id)}`, "PUT", {
      isActive: active,
      priority,
    });
  }
}
interface Pending {
  client: RouterClient;
  provider: Provider;
  state: string;
  codeVerifier: string;
  redirectUri: string;
  expires: number;
}
export class AccountAuth {
  private pending = new Map<string, Pending>();
  constructor(private clientFor: RouterClient | (() => RouterClient)) {}
  async start(provider: Provider) {
    if (!["claude", "codex"].includes(provider))
      throw new Error("Unsupported provider");
    for (const [id, p] of this.pending)
      if (p.expires < Date.now()) this.pending.delete(id);
    if (this.pending.size >= 20)
      throw new Error("Finish an existing account connection first");
    const redirectUri =
      provider === "codex"
        ? "http://localhost:1455/auth/callback"
        : "https://console.anthropic.com/oauth/code/callback";
    const client =
      typeof this.clientFor === "function" ? this.clientFor() : this.clientFor;
    const data = await client.api(
      `/api/oauth/${provider}/authorize?redirect_uri=${encodeURIComponent(redirectUri)}`,
    );
    const url = new URL(data.authUrl);
    if (
      url.protocol !== "https:" ||
      url.hostname !==
        (provider === "codex" ? "auth.openai.com" : "claude.ai") ||
      !data.state ||
      !data.codeVerifier
    )
      throw new Error("Invalid authorization response");
    const id = crypto.randomUUID();
    this.pending.set(id, {
      client,
      provider,
      state: data.state,
      codeVerifier: data.codeVerifier,
      redirectUri,
      expires: Date.now() + 10 * 60 * 1000,
    });
    return { id, provider, authUrl: data.authUrl };
  }
  async finish(id: string, callback: string): Promise<void> {
    const pending = this.pending.get(id);
    if (!pending || pending.expires < Date.now()) {
      this.pending.delete(id);
      throw new Error("Connection expired. Start again.");
    }
    let code: string | null = null,
      state: string | null = null;
    if (callback.startsWith("http")) {
      const url = new URL(callback);
      const expected = new URL(pending.redirectUri);
      if (url.origin !== expected.origin || url.pathname !== expected.pathname)
        throw new Error("Unexpected callback URL");
      code = url.searchParams.get("code");
      state = url.searchParams.get("state");
    } else if (pending.provider === "claude") {
      [code, state] = callback.split("#");
    }
    if (!code || state !== pending.state)
      throw new Error("Callback state does not match this connection");
    // Single-use even on upstream failure; a fresh login avoids ambiguous exchanges.
    this.pending.delete(id);
    await pending.client.api(
      `/api/oauth/${pending.provider}/exchange`,
      "POST",
      {
        code,
        state,
        redirectUri: pending.redirectUri,
        codeVerifier: pending.codeVerifier,
      },
    );
  }
}
