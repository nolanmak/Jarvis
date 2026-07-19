import { htmlToMarkdown, extractTitle } from "../markdown.js";
import { safeFetch } from "../ssrf.js";
import type { FetchOptions, Layer, LayerOutput } from "../types.js";

const DEFAULT_TIMEOUT_MS = 15000;
const UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

export class HttpLayer implements Layer {
  id = "http" as const;
  available() {
    return true;
  }
  async run(opts: FetchOptions): Promise<LayerOutput> {
    const ctl = new AbortController();
    const t = setTimeout(() => ctl.abort(), opts.timeout_ms ?? DEFAULT_TIMEOUT_MS);
    try {
      // safeFetch validates the target (and every redirect hop) against the
      // SSRF denylist, using redirect: 'manual' internally.
      const res = await safeFetch(opts.url, {
        method: "GET",
        signal: ctl.signal,
        headers: {
          "User-Agent": UA,
          Accept: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
          "Accept-Language": "en-US,en;q=0.9",
        },
      });
      const html = await res.text();
      return {
        ok: res.ok,
        reason: res.ok ? undefined : `HTTP ${res.status}`,
        status: res.status,
        final_url: res.url,
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
