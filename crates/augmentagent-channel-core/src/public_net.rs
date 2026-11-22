//! Which addresses are on the public internet.
//!
//! Anything that fetches a URL taken from inbound traffic must refuse hosts
//! that resolve to private, loopback, link-local or otherwise special
//! addresses (the daemon's own network, cloud metadata). One classifier, used
//! by every such fetch.
use std::net::{IpAddr, SocketAddr};

/// Resolve `url`'s host and return its addresses only if every one of them is
/// public, so the caller can pin the connection to them. `allow_loopback` is
/// for tests that run a server on this machine.
pub async fn public_addrs(
    url: &reqwest::Url,
    allow_loopback: bool,
) -> Result<Vec<SocketAddr>, &'static str> {
    if url.scheme() != "https" && !(allow_loopback && url.scheme() == "http") {
        return Err("scheme is not https");
    }
    let host = url.host_str().ok_or("no host")?;
    let port = url.port_or_known_default().ok_or("no port")?;
    // An IP literal (v4 or bracketed v6) parses straight through, no DNS.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{host}:{port}"))
        .await
        .map_err(|_| "dns lookup failed")?
        .collect();
    if addrs.is_empty() {
        return Err("host resolved to nothing");
    }
    let allowed = |ip: IpAddr| is_public_ip(ip) || (allow_loopback && ip.is_loopback());
    if !addrs.iter().all(|a| allowed(a.ip())) {
        return Err("host resolves to a non-public address");
    }
    Ok(addrs)
}

/// Global unicast per the IANA special-purpose address registries: every row
/// whose "Globally Reachable" column is not True is refused whole (no
/// carve-outs for the few reachable anycast members like 192.0.0.9), as is
/// the deprecated 6to4 relay block the registry leaves unmarked.
/// Hand-maintained because std's `is_global` is unstable and no dependency
/// ships a classifier; the SocialAPI crate's `public_ip_fence` test walks
/// every registry row, so a missed one fails a test rather than admitting a
/// fetch.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(a == 0 // "this network"
                || v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local() // cloud metadata is 169.254.169.254
                || (a == 100 && (64..=127).contains(&b)) // shared / CGNAT
                || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
                || (a == 192 && b == 88 && c == 99) // deprecated 6to4 relay anycast
                || v4.is_documentation()
                || (a == 198 && (b == 18 || b == 19)) // benchmarking
                || a >= 224) // multicast, reserved (240/4), broadcast
        }
        // Only 2000::/3 is global unicast, so everything outside it (loopback,
        // v4-mapped/-compatible, NAT64, discard, ULA, link-local, multicast,
        // ...) is out by construction; only the registry rows inside remain.
        IpAddr::V6(v6) => {
            let s = v6.segments();
            (s[0] & 0xe000) == 0x2000
                && !(matches!(s, [0x2001, b, ..] if b < 0x200 || b == 0xdb8) // IETF + docs
                    || matches!(s, [0x2002, ..]) // 6to4
                    || matches!(s, [0x3fff, b, ..] if b < 0x1000) // documentation
                    || matches!(s, [0x5f00, ..])) // SRv6 SIDs
        }
    }
}
