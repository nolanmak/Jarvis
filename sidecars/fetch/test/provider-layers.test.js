import test from "node:test";
import assert from "node:assert/strict";
import { FirecrawlLayer } from "../dist/layers/firecrawl.js";
import { BrightDataLayer } from "../dist/layers/brightdata.js";

const originalFetch = globalThis.fetch;

test.afterEach(() => {
  globalThis.fetch = originalFetch;
  delete process.env.FIRECRAWL_API_KEY;
  delete process.env.BRIGHTDATA_API_KEY;
  delete process.env.BRIGHTDATA_ZONE;
  delete process.env.FETCH_DRY_PROVIDERS;
});

test("Firecrawl uses the current scrape API and returns markdown content", async () => {
  process.env.FIRECRAWL_API_KEY = "fc-test-key";
  let request;
  globalThis.fetch = async (url, init) => {
    request = { url, init };
    return new Response(
      JSON.stringify({
        success: true,
        data: {
          markdown: "# Rendered article\n\nThe full JS-rendered body.",
          metadata: { sourceURL: "https://example.com/article", title: "Rendered article" },
        },
      }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  };

  const result = await new FirecrawlLayer().run({ url: "https://example.com/article" });
  const payload = JSON.parse(request.init.body);
  assert.equal(request.url, "https://api.firecrawl.dev/v2/scrape");
  assert.deepEqual(payload.formats, ["markdown", "html"]);
  assert.equal(payload.onlyMainContent, true);
  assert.equal(result.ok, true);
  assert.equal(result.markdown, "# Rendered article\n\nThe full JS-rendered body.");
  assert.equal(result.final_url, "https://example.com/article");
});

test("Bright Data extracts the body from the documented JSON response envelope", async () => {
  process.env.BRIGHTDATA_API_KEY = "bd-test-key";
  process.env.BRIGHTDATA_ZONE = "web_unlocker1";
  globalThis.fetch = async (_url, init) => {
    const payload = JSON.parse(init.body);
    assert.equal(payload.zone, "web_unlocker1");
    assert.equal(payload.format, "raw");
    assert.equal(payload.data_format, "markdown");
    return new Response(
      JSON.stringify({
        status_code: 200,
        headers: { "content-type": "text/html" },
        body: "# Unlocked article\n\nThe Cloudflare-protected body.",
      }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  };

  const result = await new BrightDataLayer().run({ url: "https://example.com/support" });
  assert.equal(result.ok, true);
  assert.equal(result.markdown, "# Unlocked article\n\nThe Cloudflare-protected body.");
  assert.equal(result.markdown.includes("status_code"), false);
});

test("Bright Data also accepts a raw HTML response", async () => {
  process.env.BRIGHTDATA_API_KEY = "bd-test-key";
  process.env.BRIGHTDATA_ZONE = "web_unlocker1";
  globalThis.fetch = async () =>
    new Response("<html><title>Support</title><body><p>Decoded address</p></body></html>", {
      status: 200,
      headers: { "content-type": "text/html" },
    });

  const result = await new BrightDataLayer().run({ url: "https://example.com/support" });
  assert.equal(result.ok, true);
  assert.equal(result.title, "Support");
  assert.match(result.markdown, /Decoded address/);
  assert.match(result.html, /Decoded address/);
});
