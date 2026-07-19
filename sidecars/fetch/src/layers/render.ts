import { chromium, type Browser } from "playwright";
import { htmlToMarkdown } from "../markdown.js";
import { assertUrlAllowed } from "../ssrf.js";
import type { FetchOptions, Layer, LayerOutput } from "../types.js";

const DEFAULT_NAV_MS = 25000;
const DEFAULT_SETTLE_MS = 1500;

export class RenderLayer implements Layer {
  id = "render" as const;
  private browser: Browser | null = null;

  available() {
    return true;
  }

  private async ensureBrowser(): Promise<Browser> {
    if (this.browser && this.browser.isConnected()) return this.browser;
    this.browser = await chromium.launch({ headless: true });
    return this.browser;
  }

  async run(opts: FetchOptions): Promise<LayerOutput> {
    let context;
    let page;
    try {
      // Validate the initial target before spinning up a browser context.
      await assertUrlAllowed(opts.url);

      const browser = await this.ensureBrowser();
      context = await browser.newContext({
        userAgent:
          "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
        viewport: { width: 1280, height: 800 },
      });
      page = await context.newPage();

      // Re-validate every navigation hop the browser performs (redirects,
      // meta/JS-driven document loads) so a redirect to loopback/RFC1918/
      // metadata is aborted before the request leaves the box. The route
      // handler runs per request; non-http(s) schemes and internal IPs are
      // aborted, everything else continues unchanged.
      await context.route("**/*", async (route) => {
        const req = route.request();
        // Only gate top-level / sub-document navigations; subresources to
        // public hosts are fine and gating all of them would break pages.
        if (req.resourceType() !== "document") {
          await route.continue();
          return;
        }
        try {
          await assertUrlAllowed(req.url());
          await route.continue();
        } catch {
          await route.abort("blockedbyclient");
        }
      });

      const resp = await page.goto(opts.url, {
        waitUntil: "networkidle",
        timeout: opts.timeout_ms ?? DEFAULT_NAV_MS,
      });

      // Defense in depth: re-validate the final resolved URL after navigation.
      await assertUrlAllowed(page.url());
      await page.waitForTimeout(opts.render_wait_ms ?? DEFAULT_SETTLE_MS);
      const html = await page.content();
      const title = await page.title().catch(() => undefined);
      return {
        ok: true,
        status: resp?.status(),
        final_url: page.url(),
        title,
        html,
        markdown: htmlToMarkdown(html),
      };
    } catch (e: any) {
      return { ok: false, reason: e?.message ?? String(e) };
    } finally {
      await page?.close().catch(() => {});
      await context?.close().catch(() => {});
    }
  }

  async shutdown(): Promise<void> {
    if (this.browser) {
      await this.browser.close().catch(() => {});
      this.browser = null;
    }
  }
}
