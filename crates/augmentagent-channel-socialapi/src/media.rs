//! Best-effort download of shared media for the DRAFT stage (#858).
//!
//! A tagged post or DM with an image reaches the prompts as a `[shared
//! media]` URL note (#573) — the model knows media exists, not what is in
//! it. This fetches the image to `/tmp/aa-img-<msgid>-0.<ext>` so the draft
//! prompt can carry an `IMAGE:` marker line ([`augmentagent_channel_core::images`]).
//! Strictly additive: any failure (a Reel, a CDN 403, a timeout, an oversize
//! body, a refused origin) returns `None` and the draft goes out with the URL
//! note alone; a download must never error the handler (#671 re-feeds).
//!
//! The URL comes off inbound traffic, so the fetch is fenced: `https` only,
//! no redirects, no proxy, host must resolve exclusively to public addresses,
//! and that resolution is pinned onto the client so a DNS rebind cannot swap
//! the target between the check and the connect.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use tracing::debug;

use augmentagent_channel_core::images::IMAGE_EXT_ALLOWLIST;

/// Per-image byte cap. Matches the Discord DM channel's image cap; a larger
/// download is skipped rather than truncated (a truncated image is corrupt).
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Whole-fetch timeout. Drafting is latency-sensitive and the marker is a
/// bonus, so a slow CDN forfeits the image rather than stalling the card.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Handler tests run the "CDN" as a wiremock on plain-http loopback; a
/// release build never relaxes the fence.
const ALLOW_LOOPBACK_HTTP: bool = cfg!(test);

/// A downloaded image that deletes itself when dropped, so the tempfile lives
/// exactly as long as the draft call that reads it — reasoner success, error
/// and early return alike (attachment traffic is continuous; a leak fills `/tmp`).
pub(crate) struct TempImage {
    pub(crate) path: PathBuf,
}

impl Drop for TempImage {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            debug!("socialapi media: remove {} failed: {e}", self.path.display());
        }
    }
}

/// Fetch `url` to `/tmp/aa-img-<msgid>-0.<ext>` when it is an image under
/// [`MAX_IMAGE_BYTES`] on a public `https` origin. `None` on any failure.
pub(crate) async fn fetch_image_to_tmp(url: &str, message_id: &str) -> Option<TempImage> {
    fetch_image_to_tmp_capped(url, message_id, MAX_IMAGE_BYTES).await
}

async fn fetch_image_to_tmp_capped(url: &str, message_id: &str, max_bytes: u64) -> Option<TempImage> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let addrs = match public_addrs(&parsed).await {
        Ok(a) => a,
        Err(why) => {
            debug!(url, "socialapi media fetch refused: {why}");
            return None;
        }
    };
    let client = fetch_client(parsed.host_str()?, &addrs)?;
    let mut resp = match client.get(parsed).send().await {
        Ok(r) => r,
        Err(e) => {
            debug!(url, "socialapi media fetch failed: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        debug!(url, status = %resp.status(), "socialapi media fetch: non-success status");
        return None;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let Some(ext) = image_ext(content_type.as_deref(), url) else {
        debug!(url, ?content_type, "socialapi media fetch: not an image, skipping");
        return None;
    };
    if resp.content_length().is_some_and(|len| len > max_bytes) {
        debug!(url, "socialapi media fetch: over the {max_bytes}-byte cap, skipping");
        return None;
    }
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() as u64 + chunk.len() as u64 > max_bytes {
                    debug!(url, "socialapi media fetch: over the {max_bytes}-byte cap, skipping");
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                debug!(url, "socialapi media fetch: read failed: {e}");
                return None;
            }
        }
    }
    let path = image_tmp_path(message_id, ext);
    match tokio::fs::write(&path, &buf).await {
        Ok(()) => Some(TempImage { path }),
        Err(e) => {
            debug!(url, "socialapi media fetch: write {} failed: {e}", path.display());
            None
        }
    }
}

/// A client that can only ever open a socket to `addrs`. No redirects: a hop
/// to another host would resolve outside the pinned answer (a CDN that
/// insists forfeits the image). No proxy, system/env ones included: a proxied
/// `CONNECT` hands the proxy the HOSTNAME to resolve, which bypasses the pin.
fn fetch_client(host: &str, addrs: &[SocketAddr]) -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(host, addrs)
        .build()
        .ok()
}

/// SSRF fence: `https` to a host that resolves EXCLUSIVELY to public
/// addresses. Returns the resolved addresses so the caller pins them.
async fn public_addrs(url: &reqwest::Url) -> Result<Vec<SocketAddr>, &'static str> {
    if url.scheme() != "https" && !(ALLOW_LOOPBACK_HTTP && url.scheme() == "http") {
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
    let allowed = |ip: IpAddr| is_public_ip(ip) || (ALLOW_LOOPBACK_HTTP && ip.is_loopback());
    if !addrs.iter().all(|a| allowed(a.ip())) {
        return Err("host resolves to a non-public address");
    }
    Ok(addrs)
}

/// Global unicast per the IANA special-purpose address registries: every row
/// whose "Globally Reachable" column is not True is refused whole (no
/// carve-outs for the few reachable anycast members like 192.0.0.9 — no
/// image CDN lives there), as is the deprecated 6to4 relay block the registry
/// leaves unmarked. Hand-maintained because std's `is_global` is unstable and
/// no dependency ships a classifier; `public_ip_fence` walks every registry
/// row, so a missed one fails a test rather than admitting a fetch.
fn is_public_ip(ip: IpAddr) -> bool {
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

/// `/tmp/aa-img-<msgid>-0.<ext>`, the path shape the wiki-ask scope guard
/// carves out (#441). Message ids are platform strings, so only their
/// alphanumerics survive — a hostile id cannot traverse out of `/tmp`.
fn image_tmp_path(message_id: &str, ext: &str) -> PathBuf {
    let safe: String = message_id.chars().filter(char::is_ascii_alphanumeric).collect();
    let safe = if safe.is_empty() { "socialapi" } else { safe.as_str() };
    PathBuf::from(format!("/tmp/aa-img-{safe}-0.{ext}"))
}

/// Allowlisted image extension for a response: the `Content-Type` decides
/// when it names an image; a generic or missing type falls back to the URL's
/// path extension; an explicit non-image type (a Reel's `video/mp4`, a login
/// page's `text/html`) is rejected regardless of what the URL ends in.
fn image_ext(content_type: Option<&str>, url: &str) -> Option<&'static str> {
    let mime = content_type
        .map(|ct| ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty());
    match mime.as_deref() {
        Some("image/png") => return Some("png"),
        Some("image/jpeg") | Some("image/jpg") => return Some("jpg"),
        Some("image/gif") => return Some("gif"),
        Some("image/webp") => return Some("webp"),
        None | Some("application/octet-stream") | Some("binary/octet-stream") => {}
        Some(_) => return None,
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    let ext = std::path::Path::new(parsed.path())
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    IMAGE_EXT_ALLOWLIST.iter().copied().find(|e| *e == ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mount_png(server: &MockServer, at: &str, resp: ResponseTemplate) {
        Mock::given(method("GET")).and(path(at)).respond_with(resp).mount(server).await;
    }

    // Happy path / 404 / non-image are covered end-to-end in `own_posts` and `inbound`.
    /// An oversize body is skipped, never truncated; a landed image exists for
    /// exactly the guard's lifetime.
    #[tokio::test]
    async fn caps_body_and_removes_file_on_drop() {
        let server = MockServer::start().await;
        let png = |body: Vec<u8>| {
            ResponseTemplate::new(200).insert_header("content-type", "image/png").set_body_bytes(body)
        };
        mount_png(&server, "/big.png", png(vec![0u8; 64])).await;
        mount_png(&server, "/ok.png", png(b"\x89PNG".to_vec())).await;
        let big = format!("{}/big.png", server.uri());
        assert!(fetch_image_to_tmp_capped(&big, "media_big", 16).await.is_none());
        assert!(!std::path::Path::new("/tmp/aa-img-mediabig-0.png").exists());
        let img = fetch_image_to_tmp(&format!("{}/ok.png", server.uri()), "drop_me").await.unwrap();
        let path = img.path.clone();
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG");
        drop(img);
        assert!(!path.exists());
    }

    /// The pinned resolution only holds when WE open the socket: through a
    /// system proxy (env `HTTPS_PROXY` & co) the proxy resolves the hostname
    /// itself. reqwest's `Client` Debug lists `proxies` only when some are
    /// configured, and a default client always carries the system matcher —
    /// the control assertion proves the probe can see one.
    #[test]
    fn fetch_client_never_uses_a_proxy() {
        let addrs = ["1.2.3.4:443".parse().unwrap()];
        let fenced = format!("{:?}", fetch_client("cdn.example", &addrs).unwrap());
        assert!(!fenced.contains("proxies"), "{fenced}");
        assert!(format!("{:?}", reqwest::Client::new()).contains("proxies"));
    }

    /// Non-public / non-https origins are refused before any request goes out
    /// (even under the test-only loopback allowance); redirects are not followed.
    #[tokio::test]
    async fn refuses_non_public_origins_and_redirects() {
        for url in ["https://169.254.169.254/x.png", "https://[::ffff:127.0.0.1]/a.png", "file:///a.png"] {
            let parsed = reqwest::Url::parse(url).unwrap();
            assert!(public_addrs(&parsed).await.is_err(), "{url}");
        }
        let server = MockServer::start().await;
        let redirect = ResponseTemplate::new(302).insert_header("location", "https://x.example/a.png");
        mount_png(&server, "/redir.png", redirect).await;
        let url = format!("{}/redir.png", server.uri());
        assert!(fetch_image_to_tmp(&url, "ssrf").await.is_none());
        assert!(!std::path::Path::new("/tmp/aa-img-ssrf-0.png").exists());
    }

    /// One address per IANA special-purpose registry row, in registry order
    /// (review rounds found 198.18/15, 240/4, then 192.88.99/24 slipping
    /// through), plus multicast; the rows marked globally reachable and
    /// ordinary unicast pass.
    #[test]
    fn public_ip_fence() {
        let bad = "0.0.0.0 10.1.2.3 100.64.0.1 127.0.0.1 169.254.169.254 172.16.0.1 192.0.0.1 \
                   192.0.2.1 192.88.99.1 192.168.1.1 198.18.0.1 198.51.100.1 203.0.113.5 224.0.0.1 \
                   240.0.0.1 255.255.255.255 :: ::1 ::ffff:1.2.3.4 ::1.2.3.4 64:ff9b::7f00:1 \
                   64:ff9b:1::1 100::1 2001::1 2001:2::1 2001:db8::1 2002::1 3fff::1 5f00::1 \
                   fd00::1 fe80::1 ff02::1";
        for ip in bad.split_whitespace() {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        let good = "1.2.3.4 192.0.1.1 192.31.196.1 192.52.193.1 192.175.48.1 198.20.0.1 \
                    2001:200::1 2600::1 2620:4f:8000::1 3fff:1000::1";
        for ip in good.split_whitespace() {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn tmp_path_strips_everything_but_alphanumerics() {
        assert_eq!(image_tmp_path("../../etc/passwd", "png"), PathBuf::from("/tmp/aa-img-etcpasswd-0.png"));
        assert_eq!(image_tmp_path("///", "gif"), PathBuf::from("/tmp/aa-img-socialapi-0.gif"));
    }

    #[test]
    fn image_ext_prefers_content_type_and_rejects_non_images() {
        assert_eq!(image_ext(Some("image/jpeg; charset=binary"), "https://x.example/a.mp4"), Some("jpg"));
        assert_eq!(image_ext(None, "https://x.example/a.GIF?x=1#f"), Some("gif"));
        assert_eq!(image_ext(Some("application/octet-stream"), "https://x.example/s.PNG?q=1"), Some("png"));
        assert_eq!(image_ext(None, "https://x.example/a.mp4"), None);
        assert_eq!(image_ext(Some("video/mp4"), "https://x.example/a.jpg"), None);
    }
}
