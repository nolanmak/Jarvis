import { chromium } from "playwright";
import { readFile, realpath, open, unlink } from "node:fs/promises";
import { join } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { checkAction, checkUrl } from "./policy.mjs";
import { forward } from "./network.mjs";

// Per-response byte cap for captured off-the-wire evidence bodies. Exported so
// tests can assert stored bodies against the literal value.
export const RESPONSE_BODY_CAP = 64 * 1024;

export class BrowserSession {
  constructor(options) {
    this.options = options;
    this.operating = false;
    this.interfered = false;
    this.closed = false;
    this.refs = new Map();
    this.networkFailures = [];
    this.networkResponses = [];
  }
  open(hosts) {
    this.opening = this.attach(hosts);
    return this.opening;
  }
  async attach(hosts) {
    this.hosts = hosts;
    const dir = await realpath(this.options.profileDirectory).catch(() => {
      throw Error("chrome_debugging_required");
    });
    const identity = createHash("sha256").update(dir).digest("hex");
    this.lock = join(
      this.options.lockDirectory,
      `newsletter-chrome-${process.getuid()}-${identity}.lock`,
    );
    let lock;
    try {
      lock = await open(this.lock, "wx", 0o600);
    } catch {
      this.lock = undefined;
      throw Error("chrome_busy_or_cleanup_required");
    }
    await lock.writeFile(JSON.stringify({ pid: process.pid }));
    await lock.close();
    try {
      const [port, path] = String(
        await readFile(join(dir, "DevToolsActivePort")),
      )
        .trim()
        .split("\n");
      if (
        !/^\d+$/.test(port) ||
        +port < 1 ||
        +port > 65535 ||
        !/^\/devtools\/browser\/[a-zA-Z0-9-]+$/.test(path)
      )
        throw Error("chrome_debugging_required");
      this.browser = await chromium.connectOverCDP(
        `ws://127.0.0.1:${port}${path}`,
        { timeout: 4000 },
      );
      if (this.closed) throw Error("session_closed");
      this.context = this.browser.contexts()[0];
      if (!this.context) throw Error("chrome_context_unavailable");
      this.page = await this.context.newPage();
      this.page.setDefaultTimeout(4000);
      this.page.setDefaultNavigationTimeout(4000);
      const cdp = await this.context.newCDPSession(this.page);
      await cdp.send("Network.setBypassServiceWorker", { bypass: true });
      await cdp.detach();
      this.page.on("popup", (p) => {
        void p.close();
      });
      this.page.on("download", (d) => {
        void d.cancel();
      });
      await this.page.routeWebSocket("**/*", (ws) => ws.close());
      await this.page.route("**/*", async (route) => {
        let response;
        try {
          response = await (this.options.forward ?? forward)(
            route.request(),
            hosts,
          );
          await route.fulfill(response);
        } catch (e) {
          const u = new URL(route.request().url());
          this.networkFailures.push({
            host: u.hostname,
            path: u.pathname,
            rpcIds: /^[a-zA-Z0-9,_-]{1,100}$/.test(
              u.searchParams.get("rpcids") ?? "",
            )
              ? u.searchParams.get("rpcids")
              : undefined,
            method: route.request().method(),
            reason: e.message,
          });
          this.networkFailures = this.networkFailures.slice(-20);
          await route.abort().catch(() => {});
          return;
        }
        // Isolated so a malformed body never falls into the failure/abort path.
        try {
          this.captureResponse(route.request(), response);
        } catch {}
      });
      await this.page.exposeBinding("__jarvisOwnerInput", () => {
        if (!this.operating) this.interfered = true;
      });
      await this.page.addInitScript(() => {
        // Deny non-HTTP transports in every task frame; HTTP requests use the
        // pinned proxy, and WebSockets are intercepted separately.
        for (const key of [
          "RTCPeerConnection",
          "webkitRTCPeerConnection",
          "WebTransport",
        ]) {
          Object.defineProperty(globalThis, key, {
            value: undefined,
            configurable: false,
            writable: false,
          });
        }
        for (const type of ["pointerdown", "keydown", "wheel"])
          addEventListener(
            type,
            (e) => {
              if (e.isTrusted) void window.__jarvisOwnerInput();
            },
            { capture: true },
          );
      });
      await this.page.bringToFront();
      this.ready = true;
    } catch (e) {
      await this.cleanup();
      throw Error(
        e.code === "ENOENT"
          ? "chrome_debugging_required"
          : "chrome_connection_unavailable",
      );
    }
  }
  async act(action, task) {
    if (this.closed) throw Error("session_closed");
    if (this.interfered) throw Error("owner_interference");
    checkAction(action, task);
    this.operating = true;
    try {
      let locator;
      if (action.kind === "press" && action.key === "Enter") {
        const target = await this.page
          .locator(":focus")
          .evaluate((el) =>
            [
              el.textContent,
              el.getAttribute("aria-label"),
              el.getAttribute("type"),
              el.getAttribute("autocomplete"),
              ...[...(el.labels ?? [])].map((l) => l.textContent),
            ]
              .filter(Boolean)
              .join(" "),
          )
          .catch(() => "");
        checkAction({ ...action, target }, task);
      }
      if (["click", "type"].includes(action.kind)) {
        const ref = this.refs.get(action.ref);
        if (!ref) throw Error("stale_element_reference");
        locator = this.page.locator(`[data-jarvis-ref="${action.ref}"]`);
        if ((await locator.count()) !== 1)
          throw Error("stale_element_reference");
        const label = await locator.evaluate((el) =>
          [
            el.textContent,
            el.getAttribute("aria-label"),
            el.getAttribute("placeholder"),
            el.getAttribute("type"),
            el.getAttribute("autocomplete"),
            ...[...(el.labels ?? [])].map((l) => l.textContent),
          ]
            .filter(Boolean)
            .join(" "),
        );
        checkAction({ ...action, target: label }, task);
        if ((await locator.getAttribute("type")) === "password")
          throw Error("login_required");
      }
      switch (action.kind) {
        case "navigate":
          checkUrl(action.url, this.hosts);
          await this.page
            .goto(action.url, { waitUntil: "domcontentloaded" })
            .catch((e) => {
              if (!String(e).includes("Timeout")) throw e;
            });
          break;
        case "click":
          await locator.click();
          break;
        case "type":
          await locator.fill(action.text);
          break;
        case "press":
          await this.page.keyboard.press(action.key);
          break;
        case "scroll":
          await this.page.mouse.wheel(0, action.delta);
          break;
        case "wait":
          await this.page.waitForTimeout(1000);
          break;
        case "snapshot":
          break;
        default:
          throw Error("operation_blocked");
      }
      await this.page.waitForTimeout(500);
      return await this.snapshot();
    } finally {
      this.operating = false;
    }
  }
  async snapshot() {
    const url = this.page.url();
    if (url !== "about:blank") checkUrl(url, this.hosts);
    const prefix = randomUUID().slice(0, 8);
    const data = await this.page.evaluate((prefix) => {
      const elements = [];
      for (const el of document.querySelectorAll(
        'a,button,input,select,textarea,[role="button"],[role="combobox"],[role="option"],[role="tab"],[role="gridcell"],[role="checkbox"]',
      )) {
        if (!el.getClientRects().length || elements.length >= 250) continue;
        const ref = prefix + "-" + elements.length;
        el.setAttribute("data-jarvis-ref", ref);
        elements.push({
          ref,
          tag: el.tagName,
          role: el.getAttribute("role"),
          label: (
            el.getAttribute("aria-label") ||
            el.innerText ||
            el.getAttribute("placeholder") ||
            ""
          ).slice(0, 250),
          value: el.type === "password" ? "" : el.value,
          type: el.getAttribute("type"),
        });
      }
      return {
        title: document.title,
        text: document.body?.innerText.slice(0, 45000) ?? "",
        elements,
        challenge: [...document.querySelectorAll("input[type=password]")].some(
          (el) => el.getClientRects().length > 0,
        )
          ? "login_required"
          : [
                ...document.querySelectorAll(
                  'iframe[src*="recaptcha"],iframe[src*="hcaptcha"],iframe[src*="challenges.cloudflare.com"]',
                ),
              ].some((el) => el.getClientRects().length > 0)
            ? "captcha_required"
            : null,
      };
    }, prefix);
    this.refs = new Map(data.elements.map((e) => [e.ref, e]));
    return {
      ...data,
      url,
      networkFailures: this.networkFailures,
      networkResponses: this.networkResponses,
      observedAt: new Date().toISOString(),
      id: randomUUID(),
      screenshot: await this.page.screenshot({
        type: "jpeg",
        quality: 65,
        timeout: 4000,
      }),
    };
  }
  // Capture the off-the-wire body of a successful XHR/fetch response for an
  // allowed host as first-class evidence. Headers/cookies are never stored and
  // the query string is dropped (path is the pathname only), so no credential
  // material survives; the body is bounded to RESPONSE_BODY_CAP bytes.
  captureResponse(request, response) {
    if (!["xhr", "fetch"].includes(request.resourceType())) return;
    const u = new URL(request.url());
    if (!this.hosts.includes(u.hostname)) return;
    const buf = Buffer.isBuffer(response.body)
      ? response.body
      : Buffer.from(String(response.body ?? ""));
    const contentType =
      response.contentType ?? response.headers?.["content-type"];
    const bodyExcerpt = buf.subarray(0, RESPONSE_BODY_CAP).toString("utf8");
    const entry = {
      host: u.hostname,
      path: u.pathname,
      status: response.status,
      contentType,
      observedAt: new Date().toISOString(),
    };
    if ((contentType ?? "").includes("json")) {
      try {
        entry.json = JSON.parse(bodyExcerpt);
      } catch {
        entry.bodyExcerpt = bodyExcerpt;
      }
    } else entry.bodyExcerpt = bodyExcerpt;
    this.networkResponses.push(entry);
    this.networkResponses = this.networkResponses.slice(-20);
  }
  close() {
    if (this.closing) return this.closing;
    this.closed = true;
    this.closing = (async () => {
      if (this.opening) {
        if (!this.ready) void this.browser?.close().catch(() => {});
        await this.opening.catch(() => {});
      }
      await this.cleanup();
    })();
    return this.closing;
  }
  async cleanup() {
    let clean = !this.page;
    try {
      if (this.page) {
        await this.page.close({ runBeforeUnload: false });
        clean = true;
      }
    } finally {
      await this.browser?.close();
      if (clean && this.lock) {
        await unlink(this.lock).catch((e) => {
          if (e.code !== "ENOENT") throw e;
        });
        this.lock = undefined;
      }
    }
  }
}
