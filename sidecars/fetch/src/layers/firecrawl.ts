import type { FetchOptions, Layer, LayerOutput } from "../types.js";
import { htmlToMarkdown } from "../markdown.js";

const ENDPOINT = "https://api.firecrawl.dev/v2/scrape";

export class FirecrawlLayer implements Layer {
  id = "firecrawl" as const;

  available(): boolean {
    return !!process.env.FIRECRAWL_API_KEY;
  }

  async run(opts: FetchOptions): Promise<LayerOutput> {
    const key = process.env.FIRECRAWL_API_KEY;
    if (!key) return { ok: false, reason: "FIRECRAWL_API_KEY not set" };
    if (process.env.FETCH_DRY_PROVIDERS === "1") {
      return { ok: false, reason: "dry-run: would have called Firecrawl" };
    }

    const ctl = new AbortController();
    const t = setTimeout(() => ctl.abort(), opts.timeout_ms ?? 60000);
    try {
      const res = await fetch(ENDPOINT, {
        method: "POST",
        signal: ctl.signal,
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${key}`,
        },
        body: JSON.stringify({
          url: opts.url,
          formats: ["markdown", "html"],
          onlyMainContent: true,
        }),
      });
      const json: any = await res.json().catch(() => ({}));
      if (!res.ok || json?.success === false) {
        return {
          ok: false,
          reason: `Firecrawl ${res.status}: ${json?.error ?? "unknown"}`,
          status: res.status,
        };
      }
      const data = json?.data ?? {};
      const html: string | undefined = data.html;
      const markdown: string = data.markdown ?? (html ? htmlToMarkdown(html) : "");
      return {
        ok: true,
        status: res.status,
        final_url: data.metadata?.sourceURL ?? data.metadata?.url ?? opts.url,
        title: data.metadata?.title,
        html,
        markdown,
      };
    } catch (e: any) {
      return { ok: false, reason: e?.message ?? String(e) };
    } finally {
      clearTimeout(t);
    }
  }
}
