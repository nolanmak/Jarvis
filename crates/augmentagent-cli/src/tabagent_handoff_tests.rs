//! nolanmak/tabagent#61 Part C. Every peer is local and synthetic; nothing
//! reaches Discord.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const LINK: &str = "http://100.101.102.103:47616/handoff/h1?t=synthetic-one-time";

#[derive(Default)]
struct FakeDiscord {
    posts: Mutex<Vec<(u64, String)>>,
    dms: Vec<u64>,
    fail: bool,
}

#[async_trait]
impl Poster for FakeDiscord {
    async fn owner_dm(&self, channel: u64) -> bool {
        self.dms.contains(&channel)
    }
    async fn post(&self, channel: u64, text: &str) -> Result<()> {
        if self.fail {
            anyhow::bail!("synthetic Discord failure");
        }
        self.posts.lock().unwrap().push((channel, text.to_string()));
        Ok(())
    }
}

fn target() -> Target {
    Target {
        default: 10,
        owner_channels: vec![10, 11],
    }
}

fn start_body(url: Option<&str>) -> serde_json::Value {
    json!({"event": "handoff.start", "origin": "https://accounts.example.com", "reason": "Verify your ID for the account",
           "url": url, "expiresAt": "2026-11-24T23:30:00.000Z", "session": "jarvis"})
}

struct Running {
    port: u16,
    discord: Arc<FakeDiscord>,
    route: PathBuf,
    _dir: tempfile::TempDir,
    shutdown: CancellationToken,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn running(discord: FakeDiscord) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let route = dir.path().join("tabagent-handoff-route.json");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let discord = Arc::new(discord);
    let hook = Arc::new(Webhook {
        secret: SECRET.into(),
        port,
        target: target(),
        route: Some(route.clone()),
        poster: discord.clone(),
    });
    let shutdown = CancellationToken::new();
    tokio::spawn(serve(listener, hook, shutdown.clone()));
    Running {
        port,
        discord,
        route,
        _dir: dir,
        shutdown,
    }
}

async fn send(port: u16, path: &str, host: Option<&str>, body: &serde_json::Value) -> u16 {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}{path}"))
        .json(body);
    if let Some(host) = host {
        request = request.header("host", host);
    }
    request.send().await.unwrap().status().as_u16()
}

#[tokio::test]
async fn a_valid_start_posts_the_link_once_to_the_owner_channel() {
    let hook = running(FakeDiscord::default()).await;
    let path = format!("{PATH_PREFIX}{SECRET}");
    assert_eq!(
        send(hook.port, &path, None, &start_body(Some(LINK))).await,
        204
    );
    let posts = hook.discord.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1);
    let (channel, text) = &posts[0];
    assert_eq!(*channel, 10, "no route: the owner's default channel");
    assert!(text.contains(&format!("<{LINK}>")), "{text}");
    assert!(
        text.contains("https://accounts.example.com") && text.contains("Verify your ID"),
        "{text}"
    );
    assert!(text.contains("expires 3:30 PM PT"), "{text}");
}

#[tokio::test]
async fn wrong_secret_host_or_method_never_posts() {
    let hook = running(FakeDiscord::default()).await;
    let good = format!("{PATH_PREFIX}{SECRET}");
    let wrong = format!("{PATH_PREFIX}{}", SECRET.replace('0', "1"));
    assert_eq!(
        send(hook.port, &wrong, None, &start_body(Some(LINK))).await,
        404
    );
    assert_eq!(
        send(hook.port, PATH_PREFIX, None, &start_body(Some(LINK))).await,
        404
    );
    assert_eq!(
        send(hook.port, "/", None, &start_body(Some(LINK))).await,
        404
    );
    // A browser page rebinding a DNS name to loopback sends its own Host.
    assert_eq!(
        send(
            hook.port,
            &good,
            Some("attacker.example:80"),
            &start_body(Some(LINK))
        )
        .await,
        403
    );
    let get = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://127.0.0.1:{}{good}", hook.port))
        .send()
        .await
        .unwrap();
    assert_eq!(get.status().as_u16(), 405);
    // Bad payloads are refused.
    let mut other = start_body(Some(LINK));
    other["event"] = json!("handoff.end");
    assert_eq!(send(hook.port, &good, None, &other).await, 400);
    for bad in [
        "javascript:alert(1)",
        "http://user:pw@100.64.0.1/x",
        "not a url",
    ] {
        assert_eq!(
            send(hook.port, &good, None, &start_body(Some(bad))).await,
            400,
            "{bad}"
        );
    }
    let huge =
        json!({"event": "handoff.start", "origin": "x", "reason": "r".repeat(20_000), "url": null});
    assert_eq!(send(hook.port, &good, None, &huge).await, 413);
    assert!(hook.discord.posts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn discord_failure_is_reported_to_tabagent() {
    let hook = running(FakeDiscord {
        fail: true,
        ..Default::default()
    })
    .await;
    let path = format!("{PATH_PREFIX}{SECRET}");
    assert_eq!(
        send(hook.port, &path, None, &start_body(Some(LINK))).await,
        502
    );
}

#[tokio::test]
async fn a_fresh_route_sends_the_link_to_the_turns_owner_channel_only() {
    let hook = running(FakeDiscord {
        dms: vec![77],
        ..Default::default()
    })
    .await;
    let path = format!("{PATH_PREFIX}{SECRET}");
    // The owner's query channel, a DM with the owner, then a channel others can read.
    for (channel, expected) in [(11, 11), (77, 77), (99, 10)] {
        write_route(&hook.route, channel).unwrap();
        assert_eq!(
            std::fs::metadata(&hook.route).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            send(hook.port, &path, None, &start_body(Some(LINK))).await,
            204
        );
        assert_eq!(
            hook.discord.posts.lock().unwrap().last().unwrap().0,
            expected,
            "route {channel}"
        );
        assert!(!hook.route.exists(), "a route is used once");
    }
}

#[test]
fn stale_or_malformed_routes_are_ignored_and_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("route.json");
    write_route(&path, 11).unwrap();
    let written = now_ms();
    assert_eq!(take_route(&path, written + 1000), Some(11));
    write_route(&path, 11).unwrap();
    assert_eq!(take_route(&path, written + ROUTE_FRESH_MS + 60_000), None);
    assert!(!path.exists());
    std::fs::write(&path, "{not json").unwrap();
    assert_eq!(take_route(&path, written), None);
    assert!(!path.exists());
    assert_eq!(take_route(&path, written), None);
}

#[test]
fn message_without_remote_link_sends_the_owner_to_the_legion() {
    let start = parse_start(start_body(None).to_string().as_bytes()).unwrap();
    let text = message(&start);
    assert!(
        text.contains("Chrome on the Legion") && !text.contains("<http"),
        "{text}"
    );
}

#[test]
fn message_fields_cannot_break_formatting_or_grow_without_bound() {
    let mut body = start_body(Some(LINK));
    body["reason"] = json!(format!("line one\n`code` @everyone {}", "x".repeat(500)));
    body["origin"] = json!("https://evil`.example\n**bold**");
    let text = message(&parse_start(body.to_string().as_bytes()).unwrap());
    let first = text.lines().next().unwrap();
    assert_eq!(
        first,
        "**TabAgent needs you** on `https://evil.example **bold**`"
    );
    let quote = text.lines().nth(1).unwrap();
    assert!(
        quote.starts_with("> `line one code @everyone x") && quote.ends_with("…`"),
        "{quote}"
    );
    assert!(quote.chars().count() < 320);
}

#[test]
fn secret_is_created_private_once_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state").join(SECRET_FILE);
    let first = load_or_create_secret(&path).unwrap();
    assert_eq!(first.len(), 64);
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(load_or_create_secret(&path).unwrap(), first);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        load_or_create_secret(&path).is_err(),
        "a readable secret is refused"
    );
    assert_eq!(
        webhook_url(47625, &first),
        format!("http://127.0.0.1:47625/tabagent/handoff/{first}")
    );
}

#[test]
fn port_defaults_and_zero_turns_it_off() {
    assert_eq!(port(&|_| None), Some(DEFAULT_PORT));
    assert_eq!(port(&|_| Some("47700".into())), Some(47700));
    assert_eq!(port(&|_| Some("0".into())), None);
    assert_eq!(port(&|_| Some("nope".into())), None);
}

#[test]
fn target_needs_the_owner_channel() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    };
    assert!(Target::from_env(&env(&[])).is_none());
    let target = Target::from_env(&env(&[
        ("DISCORD_CHANNEL_ID", "10"),
        ("DISCORD_QUERY_CHANNEL_ID", "11"),
    ]))
    .unwrap();
    assert_eq!((target.default, target.owner_channels), (10, vec![10, 11]));
}
