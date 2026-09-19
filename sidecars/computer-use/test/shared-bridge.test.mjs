import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { chromium } from "playwright";
import { BrowserSession } from "../browser.mjs";
test(
  "newsletter and general workers share one profile lease and one upstream Chrome connection",
  { skip: !process.env.NEWSLETTER_COMPILED_ROOT },
  async (t) => {
    const root = process.env.NEWSLETTER_COMPILED_ROOT;
    const { CdpBridge } = await import(
      pathToFileURL(join(root, "workers/cdp-bridge.js"))
    );
    const { PlaywrightRuntime, resetAttachedChromeConnections } = await import(
      pathToFileURL(join(root, "workers/playwright.js"))
    );
    const dir = await mkdtemp(join(tmpdir(), "shared-general-"));
    const previous = process.env.TMPDIR;
    process.env.TMPDIR = dir;
    const profile = join(dir, "profile");
    const bridgeDirectory = await mkdtemp(join(dir, "bridge-"));
    const chrome = await chromium.launchPersistentContext(profile, {
      channel: "chrome",
      headless: true,
      args: ["--remote-debugging-port=0"],
    });
    let connections = 0;
    const bridge = new CdpBridge({
      chromeProfileDirectory: profile,
      bridgeDirectory,
      onUpstreamConnect: () => connections++,
    });
    const general = new BrowserSession({
      profileDirectory: bridgeDirectory,
      lockDirectory: dir,
      forward: async () => ({
        status: 200,
        contentType: "text/html",
        body: "<h1>General event lookup fixture with enough visible source content.</h1>",
      }),
    });
    t.after(async () => {
      await general.close();
      await resetAttachedChromeConnections();
      await bridge.stop();
      await chrome.close();
      if (previous === undefined) delete process.env.TMPDIR;
      else process.env.TMPDIR = previous;
      await rm(dir, { recursive: true, force: true });
    });
    await bridge.start();
    await bridge.ready();
    const runtime = new PlaywrightRuntime(
      {},
      { profileDirectory: bridgeDirectory, disconnectAfterTask: true },
    );
    const newsletter = await runtime.open({
      startUrl: "https://fixture.test",
      allowedHosts: ["fixture.test"],
      steps: [],
    });
    await assert.rejects(general.open(["fixture.test"]), /busy/);
    await newsletter.close();
    await general.open(["fixture.test"]);
    const observed = await general.act(
      { kind: "navigate", url: "https://fixture.test" },
      { actions: 0 },
    );
    assert.match(observed.text, /General event/);
    await assert.rejects(
      runtime.open({
        startUrl: "https://fixture.test",
        allowedHosts: ["fixture.test"],
        steps: [],
      }),
      /busy/,
    );
    await general.close();
    const again = await runtime.open({
      startUrl: "https://fixture.test",
      allowedHosts: ["fixture.test"],
      steps: [],
    });
    await again.close();
    assert.equal(connections, 1);
    assert.equal(chrome.pages().length, 1);
  },
);
