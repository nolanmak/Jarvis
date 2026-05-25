export type ErrorKind =
  | "InvalidUrl"
  | "Network"
  | "Blocked"
  | "Timeout"
  | "AllLayersFailed"
  | "ProviderError"
  | "Internal";

export class FetchError extends Error {
  constructor(
    public kind: ErrorKind,
    message: string,
    public diagnostic?: string,
  ) {
    super(message);
    this.name = "FetchError";
  }
}

export function classify(e: unknown): FetchError {
  if (e instanceof FetchError) return e;
  const msg = e instanceof Error ? e.message : String(e);
  const lower = msg.toLowerCase();

  if (lower.includes("invalid url") || lower.includes("err_invalid_url"))
    return new FetchError("InvalidUrl", msg);
  if (lower.includes("timeout") || lower.includes("etimedout"))
    return new FetchError("Timeout", msg);
  if (/\b403\b/.test(msg) || lower.includes("cloudflare") || lower.includes("captcha") || lower.includes("datadome"))
    return new FetchError("Blocked", msg);
  if (lower.includes("enotfound") || lower.includes("econnrefused") || lower.includes("network"))
    return new FetchError("Network", msg);

  return new FetchError("Internal", msg);
}
