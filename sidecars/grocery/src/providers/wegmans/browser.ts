/**
 * Playwright lifecycle for the Wegmans provider. Mirrors `giant/browser.ts`
 * shape — persistent Chrome context (so DataDome / Wegmans bot-detection
 * cookies survive across runs), optional SOCKS5 proxy via GROCERY_PROXY.
 *
 * This is a SCAFFOLD. Once a live intercept session has captured the
 * Wegmans web app's real auth flow + endpoints (see
 * `docs/research/114-second-grocery-provider.md`), the navigation target
 * and any extra init steps (CSRF token fetch, account-picker, etc.) can
 * be filled in here. The lifecycle (start/stop/getPage) is provider-
 * agnostic and already matches `GiantBrowser`.
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
  return path.join(home, ".augmentagent", "grocery", "wegmans-chrome-profile");
}

export interface WegmansBrowserOptions {
  storeBaseUrl: string;
  chromeProfile?: string;
  proxy?: string;
  headless?: boolean;
}

export class WegmansBrowser {
  private context: BrowserContext | null = null;
  private page: Page | null = null;

  constructor(private opts: WegmansBrowserOptions) {}

  async start(): Promise<void> {
    if (this.page && !this.page.isClosed()) return;

    const profile = this.opts.chromeProfile || defaultProfileDir();
    fs.mkdirSync(profile, { recursive: true });

    const args = [
      "--no-sandbox",
      "--disable-blink-features=AutomationControlled",
      ...(this.opts.proxy ? [`--proxy-server=${this.opts.proxy}`] : []),
    ];

    console.error(`[grocery/wegmans/browser] launching chrome, profile=${profile}`);
    if (this.opts.proxy) console.error(`[grocery/wegmans/browser] proxy=${this.opts.proxy}`);

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

    console.error(`[grocery/wegmans/browser] navigating to ${this.opts.storeBaseUrl}`);
    try {
      await this.page.goto(this.opts.storeBaseUrl, {
        waitUntil: "domcontentloaded",
        timeout: 30000,
      });
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new GroceryError("Internal", `initial navigation failed: ${msg}`);
    }
    console.error("[grocery/wegmans/browser] ready");
  }

  async stop(): Promise<void> {
    if (this.context) {
      await this.context.close().catch(() => {});
      this.context = null;
      this.page = null;
      console.error("[grocery/wegmans/browser] closed");
    }
  }

  getPage(): Page {
    if (!this.page || this.page.isClosed()) {
      throw new GroceryError("Internal", "browser not started; call init() first");
    }
    return this.page;
  }
}
