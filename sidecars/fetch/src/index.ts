import "dotenv/config";
import { LayeredFetcher } from "./fetcher.js";
import { FetchSocketServer } from "./server.js";

async function main() {
  const fetcher = new LayeredFetcher();
  const server = new FetchSocketServer(fetcher);
  await server.listen();
  console.error(
    `[fetch] layers: http=on, render=on, firecrawl=${!!process.env.FIRECRAWL_API_KEY}, brightdata=${
      !!process.env.BRIGHTDATA_API_KEY && !!process.env.BRIGHTDATA_ZONE
    }`,
  );

  const shutdown = async (sig: string) => {
    console.error(`[fetch] received ${sig}, shutting down`);
    await server.close().catch(() => {});
    await fetcher.shutdown().catch(() => {});
    process.exit(0);
  };
  process.on("SIGINT", () => shutdown("SIGINT"));
  process.on("SIGTERM", () => shutdown("SIGTERM"));
}

main().catch((e) => {
  console.error("[fetch] fatal:", e);
  process.exit(1);
});
