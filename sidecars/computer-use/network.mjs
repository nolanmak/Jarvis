import dns from "node:dns/promises";
import https from "node:https";
import { checkUrl, publicIp, requestAllowed } from "./policy.mjs";
export async function pinnedAddresses(
  host,
  lookup = (host) => dns.lookup(host, { all: true }),
  fixture = false,
) {
  const addresses = await lookup(host);
  if (
    !addresses.length ||
    addresses.some(
      (a) => !publicIp(a.address) && !(fixture && a.address === "127.0.0.1"),
    )
  )
    throw Error("address_blocked");
  return addresses;
}
// Route fulfillment keeps DNS resolution and the actual TCP connection in one
// trusted transport. A preflight followed by route.continue() would permit DNS
// rebinding. Cookies remain inside this process; no headers enter model evidence.
export async function forward(request, hosts, options = {}) {
  // An explicit injected fixture origin exists only for isolated TLS tests;
  // production callers and the model interface never supply these options.
  const raw = new URL(request.url());
  const fixture =
    options.fixtureOrigin === raw.origin && raw.hostname === "fixture.test";
  const u = fixture
    ? raw
    : checkUrl(
        request.url(),
        request.isNavigationRequest() &&
          request.frame() === request.frame().page().mainFrame()
          ? hosts
          : undefined,
      );
  if (!requestAllowed(u.href, request.method(), request.postDataBuffer()))
    throw Error("request_policy_blocked");
  const addresses = await pinnedAddresses(u.hostname, options.lookup, fixture);
  const selected = addresses[0];
  const headers = await request.allHeaders();
  for (const key of [
    "host",
    "connection",
    "content-length",
    "accept-encoding",
    "transfer-encoding",
  ])
    delete headers[key];
  return new Promise((resolve, reject) => {
    const req = https.request(
      u,
      {
        method: request.method(),
        headers,
        agent: false,
        ...(fixture ? { ca: options.ca } : {}),
        lookup: (_host, options, cb) =>
          options.all
            ? cb(null, [selected])
            : cb(null, selected.address, selected.family),
      },
      (res) => {
        const chunks = [];
        let size = 0;
        res.on("data", (b) => {
          size += b.length;
          if (size > 16 * 1024 * 1024) req.destroy(Error("response_too_large"));
          else chunks.push(b);
        });
        res.on("error", reject);
        res.on("end", () => {
          const h = {};
          for (const [k, v] of Object.entries(res.headers))
            if (
              v !== undefined &&
              !["transfer-encoding", "connection", "content-length"].includes(k)
            )
              h[k] = Array.isArray(v) ? v.join("\n") : v;
          // Existing service workers are bypassed on this task's CDP session;
          // prevent task documents from starting new worker network paths.
          if ((h["content-type"] ?? "").includes("text/html"))
            h["content-security-policy"] =
              (h["content-security-policy"]
                ? h["content-security-policy"] + ", "
                : "") +
              "worker-src 'none'; object-src 'none'; sandbox allow-scripts allow-same-origin allow-forms";
          resolve({
            status: res.statusCode,
            headers: h,
            body: Buffer.concat(chunks),
          });
        });
      },
    );
    req.setTimeout(4500, () => req.destroy(Error("network_timeout")));
    req.on("error", reject);
    req.end(request.postDataBuffer() ?? undefined);
  });
}
