export type LayerId = "http" | "render" | "firecrawl" | "brightdata";

export interface FetchOptions {
  url: string;
  layers?: LayerId[];
  min_quality_chars?: number;
  timeout_ms?: number;
  render_wait_ms?: number;
}

export interface LayerAttempt {
  layer: LayerId;
  ok: boolean;
  reason?: string;
  elapsed_ms: number;
}

export interface FetchResult {
  url: string;
  final_url?: string;
  status?: number;
  title?: string;
  markdown: string;
  html?: string;
  layer_used: LayerId;
  attempts: LayerAttempt[];
  elapsed_ms: number;
}

export interface LayerOutput {
  ok: boolean;
  reason?: string;
  status?: number;
  final_url?: string;
  title?: string;
  html?: string;
  markdown?: string;
}

export interface Layer {
  id: LayerId;
  available(): boolean;
  run(opts: FetchOptions): Promise<LayerOutput>;
}
