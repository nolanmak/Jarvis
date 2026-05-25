/**
 * Pluggable grocery provider interface. v1 ships Giant; later: Instacart,
 * Whole Foods. Each provider owns its own browser/session lifecycle.
 */

export interface Product {
  prodId: string | number;
  name: string;
  brand: string;
  price: number;
  regularPrice: number;
  size: string;
  inStock: boolean;
  onSale: boolean;
}

export interface CartItem {
  prodId: string | number;
  name: string;
  brand: string;
  price: number;
  regularPrice: number;
  qty: number;
  size: string;
}

export interface CartDetail {
  cartId: string | number;
  items: CartItem[];
  subtotal: number;
  tax: number;
  total: number;
  storeId: string | number;
}

export interface SessionInfo {
  authenticated: boolean;
  userId?: string | number;
  storeId?: string | number;
  firstName?: string;
}

export interface OtpChannel {
  type: string;
  maskedValue: string;
  source: string;
}

export type LoginResult =
  | { status: "success"; userId: string | number }
  | { status: "otp_sent"; channel: string; maskedValue: string; allChannels: OtpChannel[] };

export interface SearchResult {
  query: string;
  products: Product[];
  diagnostic?: string;
}

export interface BatchSearchEntry extends SearchResult {
  error?: string;
}

export interface AddItemSpec {
  productId: string | number;
  quantity: number;
}

export interface AddItemsResult {
  added: AddItemSpec[];
  errors: Array<{ productId: string | number; error: string }>;
}

export interface GroceryProvider {
  readonly id: string;
  init(): Promise<void>;
  shutdown(): Promise<void>;
  sessionCheck(): Promise<SessionInfo>;
  login(opts?: { email?: string; password?: string }): Promise<LoginResult>;
  verifyOtp(code: string, channel?: string): Promise<{ userId: string | number }>;
  search(query: string, limit?: number): Promise<SearchResult>;
  searchBatch(queries: string[], limitPerQuery?: number): Promise<BatchSearchEntry[]>;
  productsById(prodIds: Array<string | number>): Promise<Product[]>;
  cartView(): Promise<CartDetail>;
  cartAdd(items: AddItemSpec[]): Promise<AddItemsResult>;
  cartRemove(productId: string | number): Promise<void>;
}
