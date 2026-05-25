/**
 * Wegmans web/app API client — SCAFFOLD.
 *
 * The real endpoints (paths, request shapes, response shapes, anti-bot
 * headers) MUST come from a live intercept capture against the Wegmans
 * iOS/Android app or web SPA. See
 * `docs/research/114-second-grocery-provider.md` for the capture
 * checklist.
 *
 * Every function below intentionally throws "not yet implemented" until
 * the live capture lands. Signatures mirror `giant/api.ts` so the
 * `WegmansProvider` can be wired against the existing
 * `GroceryProvider` interface without any new abstraction work.
 *
 * Implementation hints (to be confirmed once captured):
 *   - Wegmans web SPA hits `https://shop.wegmans.com/api/v2/...`
 *     (the e-commerce backend is operated by Instacart; layer is
 *     similar to Giant's PRISM but with Instacart-style JSON).
 *   - Auth is typically a Wegmans Shoppers Club login → cookie-based
 *     session. Some flows may require an OTP via SMS/email; mirror
 *     Giant's `requestOtp` / `verifyOtp` pair if so.
 *   - Cart endpoints are likely RESTful: GET /cart, POST /cart/items,
 *     DELETE /cart/items/:id (confirm via intercept).
 *
 * Until then: every export here throws and the caller surfaces a
 * structured `GroceryError("Internal", ...)` upstream.
 */

import type { Page } from "playwright";
import type {
  Product,
  CartDetail,
  SessionInfo,
  OtpChannel,
  AddItemSpec,
} from "../../provider.js";

const NOT_IMPLEMENTED =
  "Wegmans provider is a scaffold — endpoint not yet implemented. " +
  "Awaiting live intercept capture; see docs/research/114-second-grocery-provider.md " +
  "and (once captured) wiki/groceries/providers/wegmans.md.";

export interface RawLoginResult {
  status: "success" | "otp_required";
  userId?: number | string;
  channels?: OtpChannel[];
}

export async function checkSession(_page: Page): Promise<SessionInfo> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function login(
  _page: Page,
  _email: string,
  _password: string,
): Promise<RawLoginResult> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function requestOtp(
  _page: Page,
  _email: string,
  _password: string,
  _channelSource: string,
): Promise<void> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function verifyOtp(
  _page: Page,
  _email: string,
  _password: string,
  _otpCode: string,
  _channelSource: string,
): Promise<{ userId: number | string }> {
  throw new Error(NOT_IMPLEMENTED);
}

export interface ProductSearch {
  products: Product[];
  diagnostic?: string;
}

export async function searchProducts(
  _page: Page,
  _userId: number | string,
  _storeId: number | string,
  _keywords: string,
  _limit = 10,
): Promise<ProductSearch> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function getProductsByIds(
  _page: Page,
  _userId: number | string,
  _storeId: number | string,
  _prodIds: Array<string | number>,
): Promise<Product[]> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function getCart(
  _page: Page,
  _userId: number | string,
  _storeId: number | string,
): Promise<CartDetail> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function addItem(
  _page: Page,
  _userId: number | string,
  _cartId: number | string,
  _productId: number | string,
  _quantity: number,
): Promise<void> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function searchBatch(
  _page: Page,
  _userId: number | string,
  _storeId: number | string,
  _queries: string[],
  _limitPerQuery = 5,
): Promise<Array<{ query: string; products: Product[]; diagnostic?: string; error?: string }>> {
  throw new Error(NOT_IMPLEMENTED);
}

export async function addItemsBatch(
  _page: Page,
  _userId: number | string,
  _storeId: number | string,
  _items: AddItemSpec[],
): Promise<{ added: AddItemSpec[]; errors: Array<{ productId: string | number; error: string }> }> {
  throw new Error(NOT_IMPLEMENTED);
}
