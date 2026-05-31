import { lookup } from "node:dns/promises";
import { isIP } from "node:net";

/**
 * Shared SSRF guard for the fetch sidecar.
 *
 * Policy: a denylist for internal / reserved address space only — public URLs
 * keep working. Before any direct fetch / navigation (and AFTER every redirect
 * hop) the target scheme is checked and the host is DNS-resolved; the request
 * is blocked if ANY resolved IP falls in a loopback / link-local / RFC1918 /
 * unique-local / metadata range.
 *
 * The URL originates from the agent acting on inbound content, so
 * prompt-injection could otherwise steer it at http://169.254.169.254/,
 * http://localhost:<port>, internal services, or file://.
 */

/** Maximum number of redirects the http layer is permitted to follow. */
export const MAX_REDIRECTS = 10;

export class SsrfBlockedError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SsrfBlockedError";
  }
}

function parseUrl(url: string): URL {
  try {
    return new URL(url);
  } catch {
    throw new SsrfBlockedError(`invalid url: ${url}`);
  }
}

/** Reject any scheme that is not http or https (notably file://, ftp://, etc.). */
function assertHttpScheme(parsed: URL): void {
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new SsrfBlockedError(`blocked non-http(s) scheme: ${parsed.protocol}`);
  }
}

/** Classify an IPv4 dotted-quad string as a blocked (reserved/internal) range. */
function isBlockedIPv4(ip: string): boolean {
  const parts = ip.split(".").map((p) => Number(p));
  if (parts.length !== 4 || parts.some((n) => Number.isNaN(n) || n < 0 || n > 255)) {
    // Not well-formed IPv4 — treat as blocked rather than risk a bypass.
    return true;
  }
  const a = parts[0] ?? 0;
  const b = parts[1] ?? 0;
  if (a === 0) return true; // 0.0.0.0/8 (incl. 0.0.0.0)
  if (a === 127) return true; // 127.0.0.0/8 loopback
  if (a === 10) return true; // 10.0.0.0/8 RFC1918
  if (a === 172 && b >= 16 && b <= 31) return true; // 172.16.0.0/12 RFC1918
  if (a === 192 && b === 168) return true; // 192.168.0.0/16 RFC1918
  if (a === 169 && b === 254) return true; // 169.254.0.0/16 link-local (incl. 169.254.169.254 metadata)
  if (a === 100 && b >= 64 && b <= 127) return true; // 100.64.0.0/10 CGNAT
  return false;
}

/** Classify an IPv6 address string as a blocked (reserved/internal) range. */
function isBlockedIPv6(ipRaw: string): boolean {
  // Strip a zone id if present (e.g. fe80::1%eth0).
  const ip = (ipRaw.split("%")[0] ?? "").toLowerCase();

  // IPv4-mapped / IPv4-compatible addresses: classify the embedded IPv4.
  const mapped = ip.match(/(?:^|:)((?:\d{1,3}\.){3}\d{1,3})$/);
  if (mapped && mapped[1]) {
    return isBlockedIPv4(mapped[1]);
  }

  if (ip === "::1") return true; // loopback
  if (ip === "::") return true; // unspecified
  if (ip.startsWith("fe80")) return true; // link-local fe80::/10
  if (ip.startsWith("fc") || ip.startsWith("fd")) return true; // unique-local fc00::/7
  // deprecated site-local fec0::/10
  if (ip.startsWith("fec") || ip.startsWith("fed") || ip.startsWith("fee") || ip.startsWith("fef")) {
    return true;
  }
  return false;
}

export function isBlockedIp(ip: string): boolean {
  const kind = isIP(ip);
  if (kind === 4) return isBlockedIPv4(ip);
  if (kind === 6) return isBlockedIPv6(ip);
  return true; // unknown format — block to be safe.
}

/**
 * Validate a single URL: enforce http(s) scheme and DNS-resolve the host,
 * blocking if ANY resolved address is in a reserved/internal range.
 * Returns the parsed URL on success; throws SsrfBlockedError otherwise.
 */
export async function assertUrlAllowed(url: string): Promise<URL> {
  const parsed = parseUrl(url);
  assertHttpScheme(parsed);

  // hostname may already be a literal IP (URL strips IPv6 brackets).
  const host = parsed.hostname;
  if (isIP(host) !== 0) {
    if (isBlockedIp(host)) {
      throw new SsrfBlockedError(`blocked internal address: ${host}`);
    }
    return parsed;
  }

  // Resolve every A/AAAA record and block if any is internal. Using `all`
  // prevents a DNS-rebinding bypass where one record is public and another
  // internal.
  let results: Array<{ address: string }>;
  try {
    results = await lookup(host, { all: true });
  } catch (err) {
    throw new SsrfBlockedError(`dns resolution failed for ${host}: ${String(err)}`);
  }
  if (results.length === 0) {
    throw new SsrfBlockedError(`no addresses resolved for ${host}`);
  }
  for (const { address } of results) {
    if (isBlockedIp(address)) {
      throw new SsrfBlockedError(`blocked internal address for ${host}: ${address}`);
    }
  }
  return parsed;
}

/**
 * SSRF-safe fetch for the http layer: disables automatic redirect following
 * (redirect: 'manual') and re-validates EVERY hop, so each intermediate
 * Location is checked against the denylist before being followed. Caps the
 * number of redirects.
 */
export async function safeFetch(initialUrl: string, init: RequestInit): Promise<Response> {
  let currentUrl = initialUrl;
  for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
    await assertUrlAllowed(currentUrl);
    const res = await fetch(currentUrl, { ...init, redirect: "manual" });

    if (res.status >= 300 && res.status < 400) {
      const location = res.headers.get("location");
      if (!location) {
        return res; // no Location — not a followable redirect.
      }
      if (hop === MAX_REDIRECTS) {
        throw new SsrfBlockedError(`too many redirects (>${MAX_REDIRECTS})`);
      }
      // Resolve relative redirects against the current URL, then re-validate.
      currentUrl = new URL(location, currentUrl).toString();
      await res.arrayBuffer().catch(() => undefined); // drain to free the socket
      continue;
    }
    return res;
  }
  throw new SsrfBlockedError(`too many redirects (>${MAX_REDIRECTS})`);
}
