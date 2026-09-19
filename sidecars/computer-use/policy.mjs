import net from "node:net";
export function publicIp(ip) {
  if (net.isIP(ip) === 6) {
    const canonical = new URL(`http://[${ip}]`).hostname.slice(1, -1);
    const [first, second] = canonical
      .split(":")
      .map((part) => parseInt(part || "0", 16));
    return (
      first >= 0x2000 &&
      first <= 0x3fff &&
      first !== 0x2002 &&
      first !== 0x3fff &&
      !(first === 0x2001 && (second < 0x200 || second === 0xdb8))
    );
  }
  if (net.isIP(ip) !== 4) return false;
  const [a, b] = ip.split(".").map(Number);
  return !(
    a === 0 ||
    a === 10 ||
    a === 127 ||
    a >= 224 ||
    (a === 169 && b === 254) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && [0, 168].includes(b)) ||
    (a === 100 && b >= 64 && b <= 127) ||
    (a === 198 && [18, 19, 51].includes(b)) ||
    (a === 203 && b === 0)
  );
}
export function checkUrl(raw, hosts) {
  const u = new URL(raw);
  if (
    u.protocol !== "https:" ||
    u.username ||
    u.password ||
    (u.port && u.port !== "443") ||
    net.isIP(u.hostname.replace(/[\[\]]/g, "")) ||
    !u.hostname.includes(".") ||
    u.hostname.endsWith(".localhost") ||
    u.hostname.endsWith(".local")
  )
    throw Error("url_blocked");
  if (hosts && !hosts.includes(u.hostname)) throw Error("host_blocked");
  return u;
}
const consequential =
  /\b(buy|book|purchase|pay|checkout|send|submit order|delete|remove account|unsubscribe|subscribe|follow|like|post|upload|download|install|sign out|log out|password|credit card)\b/i;
export function checkAction(a, t) {
  if (t.actions >= 60) throw Error("action_budget_exhausted");
  if (
    ![
      "navigate",
      "snapshot",
      "click",
      "type",
      "press",
      "scroll",
      "wait",
      "finish",
    ].includes(a.kind)
  )
    throw Error("operation_blocked");
  if (a.target && consequential.test(a.target))
    throw Error("consequential_action_requires_authorization");
  if (
    a.kind === "press" &&
    ![
      "Enter",
      "Tab",
      "Escape",
      "ArrowDown",
      "ArrowUp",
      "ArrowLeft",
      "ArrowRight",
    ].includes(a.key)
  )
    throw Error("key_blocked");
  if (a.kind === "type" && (typeof a.text !== "string" || a.text.length > 1000))
    throw Error("invalid_text");
  if (
    a.kind === "scroll" &&
    (!Number.isFinite(a.delta) || Math.abs(a.delta) > 2000)
  )
    throw Error("invalid_scroll");
}
export function requestAllowed(raw, method, body) {
  const u = new URL(raw);
  if (
    /(?:^|[\/_?&=.-])(delete|purchase|checkout|logout|unsubscribe|send|book|payment|follow|like|upload)(?:$|[\/_?&=.-])/i.test(
      u.pathname + u.search,
    )
  )
    return false;
  if (["GET", "HEAD", "OPTIONS"].includes(method)) return true;
  if (
    method === "POST" &&
    u.hostname === "www.google.com" &&
    u.pathname === "/_/FlightsFrontendUi/data/batchexecute"
  ) {
    try {
      const groups = JSON.parse(
        new URLSearchParams(String(body ?? "")).get("f.req"),
      );
      const ids = (u.searchParams.get("rpcids") ?? "").split(",");
      const reviewed = new Set(["H028ib", "tDoGIe"]);
      return (
        ids.length > 0 &&
        ids.every((id) => reviewed.has(id)) &&
        Array.isArray(groups) &&
        groups.length > 0 &&
        groups.every(
          (group) =>
            Array.isArray(group) &&
            group.length > 0 &&
            group.every(
              (call) =>
                Array.isArray(call) &&
                reviewed.has(call[0]) &&
                ids.includes(call[0]),
            ),
        )
      );
    } catch {
      return false;
    }
  }
  return (
    method === "POST" &&
    u.hostname === "www.google.com" &&
    [
      "/_/FlightsFrontendUi/data/travel.frontend.flights.FlightsFrontendService/GetShoppingResults",
      "/_/FlightsFrontendUi/data/travel.frontend.flights.FlightsFrontendService/GetBookingResults",
    ].includes(u.pathname)
  );
}
