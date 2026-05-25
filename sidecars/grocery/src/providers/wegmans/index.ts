/**
 * WegmansProvider — implements GroceryProvider against Wegmans' web/app
 * e-commerce backend. SCAFFOLD: lifecycle + interface conformance ship
 * here; the actual endpoint calls live in `./api.ts` and currently throw
 * "not yet implemented" pending a live intercept capture (see
 * `docs/research/114-second-grocery-provider.md`).
 *
 * Mirrors `GiantProvider` line-for-line so swapping providers via the
 * `GROCERY_PROVIDER` env var is the ONLY change needed at the call site.
 */

import { WegmansBrowser } from "./browser.js";
import * as api from "./api.js";
import { GroceryError } from "../../errors.js";
import type {
  AddItemSpec,
  AddItemsResult,
  BatchSearchEntry,
  CartDetail,
  GroceryProvider,
  LoginResult,
  Product,
  SearchResult,
  SessionInfo,
} from "../../provider.js";

interface WegmansEnv {
  storeBaseUrl: string;
  storeId: string;
  email?: string;
  password?: string;
  chromeProfile?: string;
  proxy?: string;
  headless?: boolean;
}

function readEnv(): WegmansEnv {
  const storeId = process.env.WEGMANS_STORE_ID;
  if (!storeId) {
    throw new GroceryError(
      "Internal",
      "WEGMANS_STORE_ID env var is required (set once the live intercept " +
        "capture confirms how Wegmans identifies the user's home store)",
    );
  }
  const headlessRaw = process.env.GROCERY_HEADLESS;
  return {
    storeBaseUrl: process.env.WEGMANS_STORE_BASE_URL || "https://shop.wegmans.com",
    storeId,
    email: process.env.WEGMANS_EMAIL,
    password: process.env.WEGMANS_PASSWORD,
    chromeProfile: process.env.GROCERY_CHROME_PROFILE,
    proxy: process.env.GROCERY_PROXY,
    headless: headlessRaw ? headlessRaw === "1" || headlessRaw === "true" : false,
  };
}

export class WegmansProvider implements GroceryProvider {
  readonly id = "wegmans";
  private env: WegmansEnv;
  private browser: WegmansBrowser;
  private userId: string | number | null = null;
  private storeId: string | number | null = null;
  private channelSource: string | null = null;

  constructor() {
    this.env = readEnv();
    this.browser = new WegmansBrowser({
      storeBaseUrl: this.env.storeBaseUrl,
      chromeProfile: this.env.chromeProfile,
      proxy: this.env.proxy,
      headless: this.env.headless,
    });
  }

  async init(): Promise<void> {
    await this.browser.start();
  }

  async shutdown(): Promise<void> {
    await this.browser.stop();
  }

  async sessionCheck(): Promise<SessionInfo> {
    const page = this.browser.getPage();
    const s = await api.checkSession(page);
    if (s.authenticated) {
      this.userId = s.userId ?? null;
      this.storeId = s.storeId ?? this.env.storeId;
    }
    return s;
  }

  async login(opts?: { email?: string; password?: string }): Promise<LoginResult> {
    const email = opts?.email ?? this.env.email;
    const password = opts?.password ?? this.env.password;
    if (!email || !password) {
      throw new GroceryError(
        "AuthRequired",
        "WEGMANS_EMAIL / WEGMANS_PASSWORD not set and no credentials passed to login()",
      );
    }
    const page = this.browser.getPage();
    const r = await api.login(page, email, password);

    if (r.status === "otp_required" && r.channels?.length) {
      const channel = r.channels[0];
      await api.requestOtp(page, email, password, channel.source);
      this.channelSource = channel.source;
      return {
        status: "otp_sent",
        channel: channel.source,
        maskedValue: channel.maskedValue,
        allChannels: r.channels,
      };
    }

    if (r.status === "success") {
      this.userId = r.userId ?? null;
      this.storeId = this.env.storeId;
      if (r.userId === undefined) {
        throw new GroceryError("Internal", "wegmans login returned success without userId");
      }
      return { status: "success", userId: r.userId };
    }

    throw new GroceryError("Internal", `unexpected login result: ${JSON.stringify(r)}`);
  }

  async verifyOtp(code: string, channel?: string): Promise<{ userId: string | number }> {
    const email = this.env.email;
    const password = this.env.password;
    if (!email || !password) {
      throw new GroceryError("AuthRequired", "WEGMANS_EMAIL / WEGMANS_PASSWORD not set");
    }
    const ch = channel ?? this.channelSource;
    if (!ch) throw new GroceryError("OTPRequired", "no OTP channel cached — call login first");
    const page = this.browser.getPage();
    const r = await api.verifyOtp(page, email, password, code, ch);
    this.userId = r.userId;
    this.storeId = this.env.storeId;
    return { userId: r.userId };
  }

  private async ensureSession(): Promise<{ userId: string | number; storeId: string | number }> {
    if (this.userId !== null && this.storeId !== null) {
      return { userId: this.userId, storeId: this.storeId };
    }
    const s = await this.sessionCheck();
    if (!s.authenticated || s.userId === undefined) {
      throw new GroceryError("AuthRequired", "not authenticated — call login");
    }
    return { userId: s.userId, storeId: s.storeId ?? this.env.storeId };
  }

  async search(query: string, limit = 5): Promise<SearchResult> {
    const { userId, storeId } = await this.ensureSession();
    const r = await api.searchProducts(this.browser.getPage(), userId, storeId, query, limit);
    return { query, products: r.products, ...(r.diagnostic ? { diagnostic: r.diagnostic } : {}) };
  }

  async searchBatch(queries: string[], limitPerQuery = 5): Promise<BatchSearchEntry[]> {
    const { userId, storeId } = await this.ensureSession();
    const raw = await api.searchBatch(this.browser.getPage(), userId, storeId, queries, limitPerQuery);
    return raw.map((r) => ({
      query: r.query,
      products: r.products,
      ...(r.diagnostic ? { diagnostic: r.diagnostic } : {}),
      ...(r.error ? { error: r.error } : {}),
    }));
  }

  async productsById(prodIds: Array<string | number>): Promise<Product[]> {
    const { userId, storeId } = await this.ensureSession();
    return api.getProductsByIds(this.browser.getPage(), userId, storeId, prodIds);
  }

  async cartView(): Promise<CartDetail> {
    const { userId, storeId } = await this.ensureSession();
    return api.getCart(this.browser.getPage(), userId, storeId);
  }

  async cartAdd(items: AddItemSpec[]): Promise<AddItemsResult> {
    const { userId, storeId } = await this.ensureSession();
    return api.addItemsBatch(this.browser.getPage(), userId, storeId, items);
  }

  async cartRemove(productId: string | number): Promise<void> {
    const { userId, storeId } = await this.ensureSession();
    const cart = await api.getCart(this.browser.getPage(), userId, storeId);
    await api.addItem(this.browser.getPage(), userId, cart.cartId, productId, 0);
  }
}
