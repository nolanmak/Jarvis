/**
 * Standalone smoke test — runs the layered fetcher in-process (no socket).
 * Usage: `npm run build && node dist/smoke.js https://example.com`
 */
import "dotenv/config";
import { LayeredFetcher } from "./fetcher.js";

async function main() {
  const url = process.argv[2] ?? "https://example.com";
  const fetcher = new LayeredFetcher();
  try {
    const result = await fetcher.fetch({ url, min_quality_chars: 200 });
    console.log(JSON.stringify(
      {
        url: result.url,
        final_url: result.final_url,
        status: result.status,
        title: result.title,
        layer_used: result.layer_used,
        markdown_len: result.markdown.length,
        markdown_preview: result.markdown.slice(0, 600),
        attempts: result.attempts,
        elapsed_ms: result.elapsed_ms,
      },
      null,
      2,
    ));
  } finally {
    await fetcher.shutdown();
  }
}

main().catch((e) => {
  console.error("smoke failed:", e);
  process.exit(1);
});
