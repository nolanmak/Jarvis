/**
 * Playwright lifecycle for the Giant provider. Persistent Chrome context
 * keeps cookies (DataDome challenge) across runs, optional SOCKS5 proxy
 * supports running through WARP.
 *
 * Ported from /tmp/grocery-repo/mcp-servers/grocery-cart/src/browser.ts and
 * adapted to live inside a long-running sidecar (init/shutdown rather than
 * lazy-init-on-first-call).
 */

import { chromium, type BrowserContext, type Page } from "playwright";
import path from "path";
import os from "os";
import fs from "fs";
import { fileURLToPath } from "url";
import { GroceryError } from "../../errors.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const STEALTH_SCRIPT = path.resolve(__dirname, "../../../scripts/stealth.js");

function defaultProfileDir(): string {
  const home = os.homedir();
  return path.join(home, ".augmentagent", "grocery", "chrome-profile");
}

export interface GiantBrowserOptions {
  storeBaseUrl: string;
  chromeProfile?: string;
  proxy?: string;
  headless?: boolean;
}

export class GiantBrowser {
  private context: BrowserContext | null = null;
  private page: Page | null = null;

  constructor(private opts: GiantBrowserOptions) {}

  async start(): Promise<void> {
    if (this.page && !this.page.isClosed()) return;

    const profile = this.opts.chromeProfile || defaultProfileDir();
    fs.mkdirSync(profile, { recursive: true });

    const args = [
      "--no-sandbox",
      "--disable-blink-features=AutomationControlled",
      ...(this.opts.proxy ? [`--proxy-server=${this.opts.proxy}`] : []),
    ];

    console.error(`[grocery/giant/browser] launching chrome, profile=${profile}`);
    if (this.opts.proxy) console.error(`[grocery/giant/browser] proxy=${this.opts.proxy}`);

    this.context = await chromium.launchPersistentContext(profile, {
      channel: "chrome",
      args,
      headless: this.opts.headless ?? false,
      ignoreDefaultArgs: ["--enable-automation"],
    });

    this.page = this.context.pages()[0] || (await this.context.newPage());

    if (fs.existsSync(STEALTH_SCRIPT)) {
      await this.page.addInitScript({ path: STEALTH_SCRIPT });
    }

    console.error(`[grocery/giant/browser] navigating to ${this.opts.storeBaseUrl}`);
    try {
      await this.page.goto(this.opts.storeBaseUrl, {
        waitUntil: "domcontentloaded",
        timeout: 30000,
      });
    } catch (e: any) {
      throw new GroceryError("Internal", `initial navigation failed: ${e?.message ?? e}`);
    }
    console.error("[grocery/giant/browser] ready");
  }

  async stop(): Promise<void> {
    if (this.context) {
      await this.context.close().catch(() => {});
      this.context = null;
      this.page = null;
      console.error("[grocery/giant/browser] closed");
    }
  }

  getPage(): Page {
    if (!this.page || this.page.isClosed()) {
      throw new GroceryError("Internal", "browser not started; call init() first");
    }
    return this.page;
  }
}
