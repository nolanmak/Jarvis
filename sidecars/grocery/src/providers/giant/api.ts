/**
 * Giant / Ahold Delhaize PRISM API client. All calls go through
 * page.evaluate(fetch) so they ride on Chrome's TLS + cookies.
 *
 * Ported from /tmp/grocery-repo/mcp-servers/grocery-cart/src/prism-api.ts
 * (BSD-style 2024 source). Endpoints unchanged; types adapted to the
 * AugmentAgent provider interface in ../../provider.ts.
 */

import type { Page } from "playwright";
import type {
  Product,
  CartDetail,
  CartItem,
  SessionInfo,
  OtpChannel,
  AddItemSpec,
} from "../../provider.js";

export interface RawLoginResult {
  status: "success" | "otp_required";
  userId?: number;
  channels?: OtpChannel[];
}

export async function checkSession(page: Page): Promise<SessionInfo> {
  const data = await page.evaluate(async () => {
    const r = await fetch("/api/v1.0/current/user", { credentials: "include" });
    if (!r.ok) return { error: r.status };
    return r.json();
  });
  if (data?.error || !data?.userId) return { authenticated: false };
  return {
    authenticated: true,
    userId: data.userId,
    storeId: data.storeId,
    firstName: data.firstName,
  };
}

export async function login(
  page: Page,
  email: string,
  password: string,
): Promise<RawLoginResult> {
  const data = await page.evaluate(
    async ([e, p]) => {
      const r = await fetch("/api/v10.0/user/login", {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "application/json" },
        credentials: "include",
        body: JSON.stringify({ loginName: e, password: p }),
      });
      return r.json();
    },
    [email, password],
  );

  const resp = data?.response ?? data;
  const code = resp?.code ?? "";

  if (code === "LOGIN_SUCCESS") return { status: "success", userId: resp.userId };

  if (code === "LOGIN_SUCCESS_OTP_DELIVERY_CHANNEL_REQUIRED") {
    const channels: OtpChannel[] = (resp.communicationChannels ?? []).map((c: any) => ({
      type: c.type ?? c.channelType,
      maskedValue: c.maskedValue,
      source: c.source,
    }));
    return { status: "otp_required", userId: resp.userId, channels };
  }

  throw new Error(`Unexpected login response code: ${code}`);
}

export async function requestOtp(
  page: Page,
  email: string,
  password: string,
  channelSource: string,
): Promise<void> {
  const data = await page.evaluate(
    async ([e, p, ch]) => {
      const r = await fetch("/api/v10.0/user/login", {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "application/json" },
        credentials: "include",
        body: JSON.stringify({ loginName: e, password: p, otp: "", otpDeliveryChannel: ch }),
      });
      return r.json();
    },
    [email, password, channelSource],
  );
  const code = (data?.response ?? data)?.code ?? "";
  if (code !== "LOGIN_SUCCESS_OTP_SENT") throw new Error(`OTP request failed: ${code}`);
}

export async function verifyOtp(
  page: Page,
  email: string,
  password: string,
  otpCode: string,
  channelSource: string,
): Promise<{ userId: number }> {
  const data = await page.evaluate(
    async ([e, p, otp, ch]) => {
      const r = await fetch("/api/v10.0/user/login", {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "application/json" },
        credentials: "include",
        body: JSON.stringify({ loginName: e, password: p, otp, otpDeliveryChannel: ch }),
      });
      return r.json();
    },
    [email, password, otpCode, channelSource],
  );
  const resp = data?.response ?? data;
  if (resp?.code !== "LOGIN_SUCCESS") throw new Error(`OTP verification failed: ${resp?.code}`);
  return { userId: resp.userId };
}

export interface ProductSearch {
  products: Product[];
  diagnostic?: string;
}

export async function searchProducts(
  page: Page,
  userId: number,
  storeId: number,
  keywords: string,
  limit = 10,
): Promise<ProductSearch> {
  const data = await page.evaluate(
    async ([uid, sid, kw]) => {
      const params = new URLSearchParams({
        keywords: kw as string,
        sort: "bestMatch asc",
        start: "0",
        flags: "true",
        nutrition: "false",
        facetExcludeFilter: "true",
        semanticSearch: "false",
        platform: "web",
        includeSponsoredProducts: "false",
        facet: "categories,brands",
      });
      const r = await fetch(`/api/v6.0/products/${uid}/${sid}?${params}`, {
        credentials: "include",
      });
      const text = await r.text();
      return { status: r.status, body: text };
    },
    [userId, storeId, keywords],
  );

  if (data.status !== 200) {
    const snippet = data.body.slice(0, 200);
    if (data.status === 409 && snippet.includes("SESSION_EXPIRED")) {
      throw new Error("SESSION_EXPIRED: search session expired (HTTP 409)");
    }
    return { products: [], diagnostic: `HTTP ${data.status}: ${snippet}` };
  }

  let parsed: any;
  try {
    parsed = JSON.parse(data.body);
  } catch {
    return { products: [], diagnostic: `non-JSON: ${data.body.slice(0, 200)}` };
  }

  const products = parsed?.response?.products ?? [];
  if (products.length === 0) {
    return {
      products: [],
      diagnostic: `0 results — keys: [${Object.keys(parsed ?? {})}], snippet: ${data.body.slice(0, 200)}`,
    };
  }

  return {
    products: products.slice(0, limit).map((p: any) => ({
      prodId: p.prodId,
      name: p.name,
      brand: p.brand ?? "",
      price: p.price,
      regularPrice: p.regularPrice,
      size: p.size ?? "",
      inStock: !(p.flags?.outOfStock ?? false),
      onSale: p.flags?.sale ?? false,
    })),
  };
}

export async function getProductsByIds(
  page: Page,
  userId: number,
  storeId: number,
  prodIds: Array<string | number>,
): Promise<Product[]> {
  const idString = prodIds.join(",");
  const data = await page.evaluate(
    async ([uid, sid, ids]) => {
      const r = await fetch(
        `/api/v5.0/products/info/${uid}/${sid}/${ids}?flags=true&extendedInfo=true&includeOutOfStock=false`,
        { credentials: "include" },
      );
      const text = await r.text();
      return { status: r.status, body: text };
    },
    [userId, storeId, idString],
  );

  if (data.status !== 200) return [];

  let parsed: any;
  try {
    parsed = JSON.parse(data.body);
  } catch {
    return [];
  }

  const products = parsed?.response?.products ?? [];
  return products.map((p: any) => ({
    prodId: p.prodId,
    name: p.name,
    brand: p.brand ?? "",
    price: p.price,
    regularPrice: p.regularPrice,
    size: p.size ?? "",
    inStock: !(p.flags?.outOfStock ?? false),
    onSale: p.flags?.sale ?? false,
  }));
}

interface CartSummary {
  cartId: number;
  serviceLocationId: number;
  status: string;
}

async function findWorkingCart(page: Page, userId: number, storeId: number): Promise<number> {
  const data = await page.evaluate(
    async ([uid]) => {
      const r = await fetch("/api/v1/cart/cartsByUserId", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ userId: uid }),
      });
      return r.json();
    },
    [userId],
  );
  const carts: CartSummary[] = data?.body?.carts ?? data?.carts ?? [];
  const cart = carts.find((c) => c.serviceLocationId === storeId && c.status === "WORKING");
  if (!cart) throw new Error(`No WORKING cart for store ${storeId}`);
  return cart.cartId;
}

export async function getCart(page: Page, userId: number, storeId: number): Promise<CartDetail> {
  const cartId = await findWorkingCart(page, userId, storeId);
  const data = await page.evaluate(
    async ([uid, cid]) => {
      const r = await fetch("/api/v1/cart/cartByCartId", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ userId: uid, cartId: String(cid), includeServiceLocation: true }),
      });
      return r.json();
    },
    [userId, cartId],
  );

  const body = data?.body ?? data;
  const items: CartItem[] = (body?.items ?? []).map((i: any) => ({
    prodId: i.prodId,
    name: i.name,
    brand: i.brand ?? "",
    price: i.price,
    regularPrice: i.regularPrice,
    qty: i.qty,
    size: i.size ?? "",
  }));

  return {
    cartId,
    items,
    subtotal: body?.cartSubTotal ?? 0,
    tax: body?.tax ?? 0,
    total: body?.grandTotal ?? 0,
    storeId: body?.serviceLocationId ?? storeId,
  };
}

export async function addItem(
  page: Page,
  userId: number,
  cartId: number,
  productId: number,
  quantity: number,
): Promise<void> {
  const data = await page.evaluate(
    async ([uid, cid, pid, qty]) => {
      const r = await fetch("/api/v1/cart/item", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          cartId: String(cid),
          productId: pid,
          quantity: qty,
          userId: uid,
        }),
      });
      return r.json();
    },
    [userId, cartId, productId, quantity],
  );

  const result = data?.result ?? data?.body?.result ?? "";
  if (result !== "SUCCESS" && result !== "success") {
    throw new Error(`Add item failed: ${JSON.stringify(data)}`);
  }
}

function sleep(ms: number) {
  return new Promise((r) => setTimeout(r, ms));
}
const jitter = () => 300 + Math.random() * 400;

export async function searchBatch(
  page: Page,
  userId: number,
  storeId: number,
  queries: string[],
  limitPerQuery = 5,
): Promise<Array<{ query: string; products: Product[]; diagnostic?: string; error?: string }>> {
  const out: Array<{ query: string; products: Product[]; diagnostic?: string; error?: string }> = [];
  for (let i = 0; i < queries.length; i++) {
    const q = queries[i];
    try {
      const r = await searchProducts(page, userId, storeId, q, limitPerQuery);
      out.push({ query: q, products: r.products, ...(r.diagnostic ? { diagnostic: r.diagnostic } : {}) });
    } catch (e: any) {
      out.push({ query: q, products: [], error: e?.message ?? String(e) });
    }
    if (i < queries.length - 1) await sleep(jitter());
  }
  return out;
}

export async function addItemsBatch(
  page: Page,
  userId: number,
  storeId: number,
  items: AddItemSpec[],
): Promise<{ added: AddItemSpec[]; errors: Array<{ productId: string | number; error: string }> }> {
  const cartId = await findWorkingCart(page, userId, storeId);
  const added: AddItemSpec[] = [];
  const errors: Array<{ productId: string | number; error: string }> = [];

  for (let i = 0; i < items.length; i++) {
    const item = items[i];
    try {
      await addItem(page, userId, cartId, Number(item.productId), item.quantity);
      added.push(item);
    } catch (e: any) {
      errors.push({ productId: item.productId, error: e?.message ?? String(e) });
    }
    if (i < items.length - 1) await sleep(jitter());
  }
  return { added, errors };
}
