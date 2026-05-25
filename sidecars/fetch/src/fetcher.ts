import { HttpLayer } from "./layers/http.js";
import { RenderLayer } from "./layers/render.js";
import { FirecrawlLayer } from "./layers/firecrawl.js";
import { BrightDataLayer } from "./layers/brightdata.js";
import { assessQuality } from "./quality.js";
import { FetchError } from "./errors.js";
import type { FetchOptions, FetchResult, Layer, LayerAttempt, LayerId, LayerOutput } from "./types.js";

const DEFAULT_ORDER: LayerId[] = ["http", "render", "firecrawl", "brightdata"];

export class LayeredFetcher {
  private layers: Map<LayerId, Layer>;
  private render: RenderLayer;

  constructor() {
    this.render = new RenderLayer();
    this.layers = new Map<LayerId, Layer>([
      ["http", new HttpLayer()],
      ["render", this.render],
      ["firecrawl", new FirecrawlLayer()],
      ["brightdata", new BrightDataLayer()],
    ]);
  }

  async shutdown(): Promise<void> {
    await this.render.shutdown();
  }

  async fetch(opts: FetchOptions): Promise<FetchResult> {
    if (!opts.url || !/^https?:\/\//i.test(opts.url)) {
      throw new FetchError("InvalidUrl", `url must start with http(s)://, got: ${opts.url}`);
    }
    const order = (opts.layers ?? DEFAULT_ORDER).filter((id) => this.layers.has(id));
    const minChars = opts.min_quality_chars ?? 400;
    const attempts: LayerAttempt[] = [];
    const t0 = Date.now();

    let best: { layer: LayerId; output: LayerOutput } | null = null;

    for (const id of order) {
      const layer = this.layers.get(id)!;
      if (!layer.available()) {
        attempts.push({ layer: id, ok: false, reason: "not configured", elapsed_ms: 0 });
        continue;
      }
      const start = Date.now();
      const output = await layer.run(opts).catch((e: any) => ({
        ok: false,
        reason: e?.message ?? String(e),
      } as LayerOutput));
      const elapsed = Date.now() - start;

      if (!output.ok) {
        attempts.push({ layer: id, ok: false, reason: output.reason, elapsed_ms: elapsed });
        continue;
      }

      const verdict = assessQuality(output.markdown ?? "", output.html ?? "", minChars);
      attempts.push({
        layer: id,
        ok: verdict.ok,
        reason: verdict.ok ? undefined : verdict.reason,
        elapsed_ms: elapsed,
      });

      if (verdict.ok) {
        return this.build(opts.url, id, output, attempts, t0);
      }
      best = { layer: id, output };
    }

    if (best) {
      return this.build(opts.url, best.layer, best.output, attempts, t0);
    }
    throw new FetchError("AllLayersFailed", `no layer returned usable content for ${opts.url}`, JSON.stringify(attempts));
  }

  private build(url: string, layer: LayerId, output: LayerOutput, attempts: LayerAttempt[], t0: number): FetchResult {
    return {
      url,
      final_url: output.final_url,
      status: output.status,
      title: output.title,
      markdown: output.markdown ?? "",
      html: output.html,
      layer_used: layer,
      attempts,
      elapsed_ms: Date.now() - t0,
    };
  }
}
