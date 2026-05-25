import type { FetchOptions, Layer, LayerOutput } from "../types.js";
import { htmlToMarkdown, extractTitle } from "../markdown.js";

const ENDPOINT = "https://api.brightdata.com/request";

export class BrightDataLayer implements Layer {
  id = "brightdata" as const;

  available(): boolean {
    return !!process.env.BRIGHTDATA_API_KEY && !!process.env.BRIGHTDATA_ZONE;
  }

  async run(opts: FetchOptions): Promise<LayerOutput> {
    const key = process.env.BRIGHTDATA_API_KEY;
    const zone = process.env.BRIGHTDATA_ZONE;
    if (!key || !zone) return { ok: false, reason: "BRIGHTDATA_API_KEY / BRIGHTDATA_ZONE not set" };
    if (process.env.FETCH_DRY_PROVIDERS === "1") {
      return { ok: false, reason: "dry-run: would have called Bright Data" };
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
        body: JSON.stringify({ zone, url: opts.url, format: "raw" }),
      });
      const html = await res.text();
      if (!res.ok) {
        return { ok: false, reason: `Bright Data ${res.status}`, status: res.status };
      }
      return {
        ok: true,
        status: res.status,
        final_url: opts.url,
        title: extractTitle(html),
        html,
        markdown: htmlToMarkdown(html),
      };
    } catch (e: any) {
      return { ok: false, reason: e?.message ?? String(e) };
    } finally {
      clearTimeout(t);
    }
  }
}
