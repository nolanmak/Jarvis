/**
 * Grocery sidecar entrypoint. Picks a provider from GROCERY_PROVIDER
 * (default: giant), wires it into the NDJSON socket server, handles
 * graceful shutdown.
 */

import "dotenv/config";
import { GiantProvider } from "./providers/giant/index.js";
import { GrocerySocketServer } from "./server.js";
import type { GroceryProvider } from "./provider.js";

function pickProvider(): GroceryProvider {
  const id = (process.env.GROCERY_PROVIDER || "giant").toLowerCase();
  switch (id) {
    case "giant":
      return new GiantProvider();
    default:
      throw new Error(`unknown GROCERY_PROVIDER: ${id}`);
  }
}

async function main() {
  const provider = pickProvider();
  console.error(`[grocery] provider=${provider.id}`);
  await provider.init();
  const server = new GrocerySocketServer(provider);
  await server.listen();

  const shutdown = async (sig: string) => {
    console.error(`[grocery] received ${sig}, shutting down`);
    await server.close().catch(() => {});
    await provider.shutdown().catch(() => {});
    process.exit(0);
  };
  process.on("SIGINT", () => shutdown("SIGINT"));
  process.on("SIGTERM", () => shutdown("SIGTERM"));
}

main().catch((e) => {
  console.error("[grocery] fatal:", e);
  process.exit(1);
});
