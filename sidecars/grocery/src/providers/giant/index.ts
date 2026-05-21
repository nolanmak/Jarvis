/**
 * GiantProvider — implements GroceryProvider against Giant Food Stores
 * (Ahold Delhaize PRISM API). Owns the browser lifecycle and a small
 * session cache (userId/storeId, OTP channel from the last login).
 */

import { GiantBrowser } from "./browser.js";
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

interface GiantEnv {
  storeBaseUrl: string;
  storeId: number;
  email?: string;
  password?: string;
  chromeProfile?: string;
  proxy?: string;
  headless?: boolean;
}

function readEnv(): GiantEnv {
  const storeIdRaw = process.env.GIANT_STORE_ID;
  if (!storeIdRaw) throw new GroceryError("Internal", "GIANT_STORE_ID env var is required");
  const storeId = Number.parseInt(storeIdRaw, 10);
  if (!Number.isFinite(storeId)) throw new GroceryError("Internal", `GIANT_STORE_ID is not numeric: ${storeIdRaw}`);
  const headlessRaw = process.env.GROCERY_HEADLESS;
  return {
    storeBaseUrl: process.env.GIANT_STORE_BASE_URL || "https://giantfoodstores.com",
    storeId,
    email: process.env.GIANT_EMAIL,
    password: process.env.GIANT_PASSWORD,
    chromeProfile: process.env.GROCERY_CHROME_PROFILE,
    proxy: process.env.GROCERY_PROXY,
    headless: headlessRaw ? headlessRaw === "1" || headlessRaw === "true" : false,
  };
}

export class GiantProvider implements GroceryProvider {
  readonly id = "giant";
  private env: GiantEnv;
  private browser: GiantBrowser;
  private userId: number | null = null;
  private storeId: number | null = null;
  private channelSource: string | null = null;

  constructor() {
    this.env = readEnv();
    this.browser = new GiantBrowser({
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
      this.userId = Number(s.userId);
      this.storeId = Number(s.storeId ?? this.env.storeId);
    }
    return s;
  }

  async login(opts?: { email?: string; password?: string }): Promise<LoginResult> {
    const email = opts?.email ?? this.env.email;
    const password = opts?.password ?? this.env.password;
    if (!email || !password) {
      throw new GroceryError(
        "AuthRequired",
        "GIANT_EMAIL / GIANT_PASSWORD not set and no credentials passed to login()",
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
      return { status: "success", userId: r.userId! };
    }

    throw new GroceryError("Internal", `unexpected login result: ${JSON.stringify(r)}`);
  }

  async verifyOtp(code: string, channel?: string): Promise<{ userId: string | number }> {
    const email = this.env.email;
    const password = this.env.password;
    if (!email || !password) {
      throw new GroceryError("AuthRequired", "GIANT_EMAIL / GIANT_PASSWORD not set");
    }
    const ch = channel ?? this.channelSource;
    if (!ch) throw new GroceryError("OTPRequired", "no OTP channel cached — call login first");
    const page = this.browser.getPage();
    const r = await api.verifyOtp(page, email, password, code, ch);
    this.userId = r.userId;
    this.storeId = this.env.storeId;
    return { userId: r.userId };
  }

  private async ensureSession(): Promise<{ userId: number; storeId: number }> {
    if (this.userId && this.storeId) return { userId: this.userId, storeId: this.storeId };
    const s = await this.sessionCheck();
    if (!s.authenticated || !s.userId) {
      throw new GroceryError("AuthRequired", "not authenticated — call login");
    }
    return { userId: Number(s.userId), storeId: Number(s.storeId ?? this.env.storeId) };
  }

  private async withReauth<T>(fn: () => Promise<T>): Promise<T> {
    try {
      return await fn();
    } catch (e: any) {
      if (!String(e?.message ?? e).includes("SESSION_EXPIRED")) throw e;
      console.error("[grocery/giant] session expired, attempting silent re-login");
      const page = this.browser.getPage();
      if (!this.env.email || !this.env.password) throw e;
      const r = await api.login(page, this.env.email, this.env.password);
      if (r.status !== "success") {
        throw new GroceryError("OTPRequired", "re-login required OTP; cannot auto-recover");
      }
      this.userId = r.userId ?? null;
      this.storeId = this.env.storeId;
      return fn();
    }
  }

  async search(query: string, limit = 5): Promise<SearchResult> {
    return this.withReauth(async () => {
      const { userId, storeId } = await this.ensureSession();
      const r = await api.searchProducts(this.browser.getPage(), userId, storeId, query, limit);
      return { query, products: r.products, ...(r.diagnostic ? { diagnostic: r.diagnostic } : {}) };
    });
  }

  async searchBatch(queries: string[], limitPerQuery = 5): Promise<BatchSearchEntry[]> {
    return this.withReauth(async () => {
      const { userId, storeId } = await this.ensureSession();
      const raw = await api.searchBatch(this.browser.getPage(), userId, storeId, queries, limitPerQuery);
      return raw.map((r) => ({
        query: r.query,
        products: r.products,
        ...(r.diagnostic ? { diagnostic: r.diagnostic } : {}),
        ...(r.error ? { error: r.error } : {}),
      }));
    });
  }

  async productsById(prodIds: Array<string | number>): Promise<Product[]> {
    return this.withReauth(async () => {
      const { userId, storeId } = await this.ensureSession();
      return api.getProductsByIds(this.browser.getPage(), userId, storeId, prodIds);
    });
  }

  async cartView(): Promise<CartDetail> {
    return this.withReauth(async () => {
      const { userId, storeId } = await this.ensureSession();
      return api.getCart(this.browser.getPage(), userId, storeId);
    });
  }

  async cartAdd(items: AddItemSpec[]): Promise<AddItemsResult> {
    return this.withReauth(async () => {
      const { userId, storeId } = await this.ensureSession();
      return api.addItemsBatch(this.browser.getPage(), userId, storeId, items);
    });
  }

  async cartRemove(productId: string | number): Promise<void> {
    return this.withReauth(async () => {
      const { userId, storeId } = await this.ensureSession();
      const cart = await api.getCart(this.browser.getPage(), userId, storeId);
      await api.addItem(this.browser.getPage(), userId, Number(cart.cartId), Number(productId), 0);
    });
  }
}
