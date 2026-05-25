/**
 * Structured error kinds returned through the NDJSON socket.
 * Mirrors sidecars/browser/sidecar.py's error.kind taxonomy plus a few
 * grocery-specific kinds.
 */
export type ErrorKind =
  | "AuthRequired"
  | "OTPRequired"
  | "OutOfStock"
  | "NotFound"
  | "RateLimited"
  | "Timeout"
  | "CartError"
  | "Internal";

export class GroceryError extends Error {
  constructor(
    public kind: ErrorKind,
    message: string,
    public diagnostic?: string,
  ) {
    super(message);
    this.name = "GroceryError";
  }
}

export function classify(e: unknown): GroceryError {
  if (e instanceof GroceryError) return e;
  const msg = e instanceof Error ? e.message : String(e);
  const lower = msg.toLowerCase();

  if (msg.includes("Not authenticated") || /\b401\b/.test(msg) || msg.includes("SESSION_EXPIRED"))
    return new GroceryError("AuthRequired", msg);
  if (lower.includes("otp") || msg.includes("otp_required"))
    return new GroceryError("OTPRequired", msg);
  if (/\b403\b/.test(msg) || lower.includes("datadome") || lower.includes("captcha"))
    return new GroceryError("Internal", "DataDome / bot challenge detected. Browser session may need refreshing.", msg);
  if (lower.includes("timeout") || msg.includes("ETIMEDOUT"))
    return new GroceryError("Timeout", msg);
  if (msg.includes("No WORKING cart") || msg.includes("Add item failed"))
    return new GroceryError("CartError", msg);
  if (lower.includes("not found") || lower.includes("zero results"))
    return new GroceryError("NotFound", msg);
  if (/\b429\b/.test(msg) || lower.includes("rate limit"))
    return new GroceryError("RateLimited", "Too many requests. Wait a moment and retry.");

  return new GroceryError("Internal", msg);
}
