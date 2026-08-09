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
        body: JSON.stringify({ zone, url: opts.url, format: "raw", data_format: "markdown" }),
      });
      const contentType = res.headers.get("content-type") ?? "";
      const raw = await res.text();
      let body = raw;
      let providerStatus: number | undefined;
      if (contentType.toLowerCase().includes("application/json")) {
        try {
          const json: any = JSON.parse(raw);
          providerStatus = typeof json?.status_code === "number" ? json.status_code : undefined;
          body =
            typeof json?.body === "string"
              ? json.body
              : typeof json?.data?.body === "string"
                ? json.data.body
                : typeof json?.data === "string"
                  ? json.data
                  : raw;
        } catch {
          // Some zones return raw content with an inaccurate JSON content type.
        }
      }
      if (!res.ok) {
        return { ok: false, reason: `Bright Data ${res.status}: ${body.slice(0, 200)}`, status: res.status };
      }
      const looksHtml = /<(?:html|body|main|article|div|p|title)\b/i.test(body);
      return {
        ok: true,
        status: providerStatus ?? res.status,
        final_url: opts.url,
        title: looksHtml ? extractTitle(body) : undefined,
        html: looksHtml ? body : undefined,
        markdown: looksHtml ? htmlToMarkdown(body) : body.trim(),
      };
    } catch (e: any) {
      return { ok: false, reason: e?.message ?? String(e) };
    } finally {
      clearTimeout(t);
    }
  }
}
