//! Lua subscribers, driven over a real socket.
//!
//! The theme here is that a subscriber is a script: it fetches what it wants,
//! reshapes it into proxy tables, and returns them — while staying inside a
//! sandbox whose only way out is `fetch`.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use std::sync::Arc;

use proxygate::config::{Config, SubscriberConfig};
use proxygate::pool::{HealthPolicy, HealthUpdate, ProxyPool};
use proxygate::selector::SelectionOptions;
use proxygate::subscriber::{EgressPolicy, SubscriberSet};

/// A JSON envelope in the shape a lot of panel APIs use.
const ENVELOPE: &str = r#"{
  "code": 200,
  "data": {
    "items": [
      {"ip": "10.0.0.1", "port": 8080, "protocol": "http"},
      {"ip": "10.0.0.2", "port": 1080, "protocol": "socks4"},
      {"ip": "10.0.0.3", "port": 1080, "protocol": "socks5"}
    ]
  }
}"#;

/// Creates a scratch directory for a test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("proxygate-it-lua-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn lua(name: &str, code: &str) -> SubscriberConfig {
    SubscriberConfig {
        name: name.to_string(),
        lua_code: Some(code.to_string()),
        lua_file: None,
        timeout: Some(Duration::from_secs(10)),
        limit: None,
        enabled: true,
        via: Default::default(),
        // Filled in per test through `with_param`.
        params: BTreeMap::new(),
    }
}

/// Adds a script global (a key ProxyGate does not recognise) to a subscriber.
fn with_param(mut subscriber: SubscriberConfig, key: &str, value: &str) -> SubscriberConfig {
    subscriber
        .params
        .insert(key.to_string(), serde_yaml::Value::from(value));
    subscriber
}

/// Renders the first subscriber's outcome in canonical form.
async fn fetch(config: Config) -> Vec<String> {
    let set = SubscriberSet::new(&config).expect("subscriber set");
    let outcome = &set.fetch_all().await[0];
    assert!(outcome.ok(), "unexpected error: {:?}", outcome.error);
    rendered(outcome)
}

fn rendered(outcome: &proxygate::subscriber::FetchOutcome) -> Vec<String> {
    outcome
        .proxies
        .iter()
        .map(|url| proxygate::model::render_url(url, true))
        .collect()
}

/// 用只含该订阅源的配置跑一次，返回第一个 outcome。
async fn run_one(subscriber: SubscriberConfig) -> proxygate::subscriber::FetchOutcome {
    let set = SubscriberSet::new(&config_with(vec![subscriber])).expect("subscriber set");
    let mut outcomes = set.fetch_all().await;
    outcomes.remove(0)
}

fn config_with(subscribers: Vec<SubscriberConfig>) -> Config {
    Config {
        subscribers,
        ..Config::default()
    }
}

#[tokio::test]
async fn a_script_fetches_and_reshapes_an_api() {
    let address = common::fake_http_server(ENVELOPE).await;

    // The script reads the protocol of each entry, keeps HTTP as HTTP, turns
    // socks5 into socks5h (so the proxy resolves names), and never returns the
    // socks4-only entry.
    let code = r#"
        local body = fetch_json(target_url)
        local result = {}
        for _, item in ipairs(body.data.items) do
          table.insert(result, {
            type = item.protocol,
            ip = item.ip,
            port = item.port,
            auth = item.auth or "",
          })
        end
        return result
    "#;

    let subscriber = with_param(
        lua("panel", code),
        "target_url",
        &format!("http://{address}/list"),
    );
    let rendered = fetch(config_with(vec![subscriber])).await;

    assert_eq!(
        rendered,
        vec![
            "http://10.0.0.1:8080".to_string(),
            // The socks4-only entry is skipped, not handed out as HTTP.
            "socks5h://10.0.0.3:1080".to_string(),
        ]
    );
}

#[tokio::test]
async fn credentials_survive_the_round_trip() {
    let subscriber = lua(
        "auth",
        r#"
        return {
          { type = "http", ip = "10.0.0.1", port = 8080, auth = "user:pa ss" },
          { type = "socks5h", ip = "10.0.0.2", port = 1080, auth = "onlyuser" },
        }
    "#,
    );
    let rendered = fetch(config_with(vec![subscriber])).await;
    assert_eq!(
        rendered,
        vec![
            // The space is percent-encoded rather than silently dropped.
            "http://user:pa%20ss@10.0.0.1:8080".to_string(),
            "socks5h://onlyuser@10.0.0.2:1080".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_script_can_page_through_an_api() {
    // The server answers each page with a different proxy, so the assertion
    // proves the loop really issued one request per page.
    let address = common::spawn_server(|mut stream| async move {
        use tokio::io::AsyncWriteExt;

        let Some(head) = common::read_head(&mut stream).await else {
            return;
        };
        let page: u32 = head
            .lines()
            .next()
            .and_then(|line| line.split("page=").nth(1))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|page| page.parse().ok())
            .unwrap_or(1);
        let body = format!("10.1.1.{page}:8080");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    })
    .await;

    let code = r#"
        local result = {}
        for page = 1, 3 do
          local body = fetch(target_url .. "?page=" .. page)
          table.insert(result, body)
        end
        return result
    "#;
    let subscriber = with_param(
        lua("paged", code),
        "target_url",
        &format!("http://{address}/p"),
    );

    let rendered = fetch(config_with(vec![subscriber])).await;
    assert_eq!(
        rendered,
        vec![
            "http://10.1.1.1:8080".to_string(),
            "http://10.1.1.2:8080".to_string(),
            "http://10.1.1.3:8080".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_script_can_come_from_a_file() {
    let dir = scratch("file");
    let path = dir.join("scraper.lua");
    std::fs::write(
        &path,
        "return { { type = 'http', ip = '10.2.2.2', port = 3128 } }\n",
    )
    .expect("write script");

    let mut subscriber = lua("from-file", "ignored");
    subscriber.lua_code = None;
    subscriber.lua_file = Some(path.clone());

    let rendered = fetch(config_with(vec![subscriber])).await;
    assert_eq!(rendered, vec!["http://10.2.2.2:3128".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_script_file_is_a_subscriber_error() {
    let mut subscriber = lua("missing", "return {}");
    subscriber.lua_code = None;
    subscriber.lua_file = Some(PathBuf::from("/definitely/not/here.lua"));

    let set = SubscriberSet::new(&config_with(vec![subscriber])).expect("subscriber set");
    let outcome = &set.fetch_all().await[0];
    assert!(!outcome.ok());
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("here.lua"),
        "{:?}",
        outcome.error
    );
}

#[tokio::test]
async fn a_broken_script_does_not_hide_the_others() {
    // Failure isolation: one source's script erroring must not stop the rest of
    // the refresh.
    let good = lua(
        "good",
        "return { { type = 'http', ip = '10.9.9.9', port = 8080 } }",
    );
    let bad = lua("bad", "error('the panel changed its schema')");

    let set = SubscriberSet::new(&config_with(vec![good, bad])).expect("subscriber set");
    let outcomes = set.fetch_all().await;
    assert_eq!(outcomes.len(), 2);

    let bad = outcomes
        .iter()
        .find(|o| o.name == "bad")
        .expect("bad outcome");
    assert!(!bad.ok());
    assert!(bad.error.as_deref().unwrap_or_default().contains("schema"));

    let good = outcomes
        .iter()
        .find(|o| o.name == "good")
        .expect("good outcome");
    assert!(good.ok(), "{:?}", good.error);
    assert_eq!(good.count(), 1);
}

#[tokio::test]
async fn malformed_entries_are_reported_without_killing_the_source() {
    let subscriber = lua(
        "mixed",
        r#"
        return {
          { type = "http", ip = "10.0.0.1", port = 8080 },
          { type = "http", port = 8080 },
          { type = "http", ip = "10.0.0.2", port = "not a port" },
        }
    "#,
    );
    let set = SubscriberSet::new(&config_with(vec![subscriber])).expect("subscriber set");
    let outcome = &set.fetch_all().await[0];

    assert!(outcome.ok(), "{:?}", outcome.error);
    assert_eq!(rendered(outcome), vec!["http://10.0.0.1:8080".to_string()]);
    assert_eq!(outcome.rejected.len(), 2, "{:?}", outcome.rejected);
    assert!(
        outcome.rejected[0].contains("missing `ip`"),
        "{:?}",
        outcome.rejected
    );
    assert!(
        outcome.rejected[1].contains("port"),
        "{:?}",
        outcome.rejected
    );
}

#[tokio::test]
async fn scripts_are_isolated_from_each_other() {
    // Each subscriber gets a fresh Lua state, so a script cannot leak a global
    // into another one — even when both run in the same refresh.
    let writer = lua(
        "writer",
        "_G.shared = 'leaked'\nreturn { { type = 'http', ip = '10.3.3.3', port = 8080 } }",
    );
    let reader = lua(
        "reader",
        "local host = _G.shared == nil and '10.4.4.4' or '10.5.5.5'\n\
         return { { type = 'http', ip = host, port = 8080 } }",
    );

    let set = SubscriberSet::new(&config_with(vec![writer, reader])).expect("subscriber set");
    let outcomes = set.fetch_all().await;

    let by_name = |name: &str| -> Vec<String> {
        let outcome = outcomes.iter().find(|o| o.name == name).expect("outcome");
        assert!(outcome.ok(), "{name}: {:?}", outcome.error);
        rendered(outcome)
    };

    assert_eq!(by_name("writer"), vec!["http://10.3.3.3:8080".to_string()]);
    // The reader never saw the writer's global.
    assert_eq!(by_name("reader"), vec!["http://10.4.4.4:8080".to_string()]);
}

// ---------------------------------------------------------------------------
// 出厂的示例配置里那些脚本，拿真实页面的裁剪片段跑一遍。
//
// 这两个测试的意义在于：脚本是写在 config.example.yaml 里的，改了配置却把解析
// 规则改坏，只有这里会红。
// ---------------------------------------------------------------------------

/// 从示例配置里取出某个订阅源（脚本与参数一起），并把参数指向测试服务器。
fn shipped_subscriber(name: &str, base_url: &str, delay: &str) -> SubscriberConfig {
    let config: Config =
        serde_yaml::from_str(proxygate::config::EXAMPLE_CONFIG).expect("the example config parses");
    let mut subscriber = config
        .subscribers
        .into_iter()
        .find(|subscriber| subscriber.name() == name)
        .unwrap_or_else(|| panic!("the example config has a `{name}` subscriber"));
    subscriber
        .params
        .insert("base_url".into(), serde_yaml::Value::from(base_url));
    // 测试里不真的等：`sleep(0)` 直接返回。
    subscriber
        .params
        .insert("delay".into(), serde_yaml::Value::from(delay));
    subscriber
}

/// zdaye 免费代理页的真实标记片段（见 tests/fixtures/zdaye_free.html）。
const ZDAYE_PAGE: &str = include_str!("fixtures/zdaye_free.html");

#[tokio::test]
async fn an_empty_result_is_not_an_error() {
    // 一个什么都没拿到的脚本（比如来源被限流了）应当安静地返回空，
    // 而不是报错、也不是把空表当成一条坏条目。
    let outcome = run_one(lua("nothing", "return {}")).await;
    assert!(outcome.ok(), "{:?}", outcome.error);
    assert_eq!(outcome.count(), 0);
    assert!(outcome.rejected.is_empty(), "{:?}", outcome.rejected);
}

#[tokio::test]
async fn the_shipped_zdaye_script_parses_the_real_markup() {
    let address = common::fake_http_server(ZDAYE_PAGE).await;
    let subscriber = shipped_subscriber("zdaye", &format!("http://{address}/free/"), "0");

    let outcome = run_one(subscriber).await;
    assert!(outcome.ok(), "{:?}", outcome.error);
    assert_eq!(outcome.error, None);
    // max_pages 默认 3：同一个 fixture 被当成三页，所以每条出现三次。
    assert_eq!(outcome.count(), 9, "{:?}", rendered(&outcome));
    assert_eq!(outcome.skipped, 0, "两个 HTTP(S) 加一个 SOCKS5 都该留下");

    let mut hosts: Vec<String> = outcome
        .proxies
        .iter()
        .map(|url| url.host_str().unwrap_or_default().to_string())
        .collect();
    hosts.sort();
    hosts.dedup();
    assert_eq!(
        hosts,
        vec!["103.152.112.162", "139.159.97.82", "47.92.82.167"],
        "页面里的三条 IP 一条都不能少"
    );

    // HTTPS 归一成 http（"能 CONNECT 到 HTTPS"），SOCKS5 升级成 socks5h。
    let rendered = rendered(&outcome);
    assert!(
        rendered.contains(&"http://47.92.82.167:9999".to_string()),
        "{rendered:?}"
    );
    assert!(
        rendered.contains(&"http://139.159.97.82:10900".to_string()),
        "{rendered:?}"
    );
    assert!(
        rendered.contains(&"socks5h://103.152.112.162:1080".to_string()),
        "{rendered:?}"
    );
}

#[tokio::test]
async fn the_shipped_zdaye_script_survives_a_blocked_page() {
    // 第一页正常，之后几页回 500（WAF 拦人时就是这样）：脚本要 `pcall` 掉，
    // 把已经拿到的交出去，而不是让整个订阅源失败。
    let address = common::spawn_server(|mut stream| async move {
        use tokio::io::AsyncWriteExt;

        let Some(head) = common::read_head(&mut stream).await else {
            return;
        };
        let path = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        // `/free/` 是第 1 页，`/free/2/` 之后一律拦截。
        let (status, body) = if path == "/free/" {
            ("200 OK", ZDAYE_PAGE)
        } else {
            ("500 Server too busy", "")
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    })
    .await;

    let subscriber = shipped_subscriber("zdaye", &format!("http://{address}/free/"), "0");
    let outcome = run_one(subscriber).await;

    assert!(outcome.ok(), "被拦的页面不该让整个来源失败");
    assert_eq!(
        outcome.count(),
        3,
        "只有第 1 页的三条应该留下：{:?}",
        rendered(&outcome)
    );
}

// ---------------------------------------------------------------------------
// 出口：浏览器 UA，以及"直连抓不动就借池子里的健康代理"。
// ---------------------------------------------------------------------------

/// 一个只认绝对形式请求（也就是"经过代理"）的来源服务器：
/// 直连是 `GET /list`，经过代理是 `GET http://host:port/list`。
///
/// 用它来区分"这条请求到底走没走代理"，比去数代理那边收到了几次更直接。
async fn origin_only_behind_proxy(body: &'static str) -> std::net::SocketAddr {
    common::spawn_server(move |mut stream| async move {
        use tokio::io::AsyncWriteExt;

        let Some(head) = common::read_head(&mut stream).await else {
            return;
        };
        let request_line = head.lines().next().unwrap_or_default().to_string();
        let (status, payload) = if request_line.starts_with("GET http://") {
            ("200 OK", body)
        } else {
            ("500 Blocked", "direct access is not allowed")
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    })
    .await
}

/// 一个"健康代理"：把任何请求都当成自己的活，回同样的内容。
async fn pretend_proxy(body: &'static str) -> std::net::SocketAddr {
    common::fake_http_server(body).await
}

/// 造一个池子，里面只有一个指向 `proxy` 的健康代理。
fn pool_with_proxy(proxy: std::net::SocketAddr) -> Arc<ProxyPool> {
    let pool = Arc::new(ProxyPool::new());
    let (id, _) =
        pool.insert(proxygate::model::normalize(&format!("http://{proxy}")).expect("proxy url"));
    pool.apply_health_pass(
        &[(
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(5)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )],
        &HealthPolicy::default(),
    );
    pool
}

/// 用指定的出口策略跑一次脚本，允许指定池子。
async fn run_with_egress(
    mut subscriber: SubscriberConfig,
    via: EgressPolicy,
    pool: Option<Arc<ProxyPool>>,
) -> proxygate::subscriber::FetchOutcome {
    subscriber.via = via;
    let config = config_with(vec![subscriber]);
    let mut set = SubscriberSet::new(&config).expect("subscriber set");
    if let Some(pool) = pool {
        set = set.with_egress(pool, SelectionOptions::default());
    }
    set.fetch_all().await.remove(0)
}

#[tokio::test]
async fn the_default_user_agent_is_an_ordinary_browser() {
    // 抓公开列表时用 `proxygate/x.y.z` 敲门会被不少站点直接拒掉。
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let recorder = seen.clone();
    let address = common::spawn_server(move |mut stream| {
        let recorder = recorder.clone();
        async move {
            use tokio::io::AsyncWriteExt;

            let Some(head) = common::read_head(&mut stream).await else {
                return;
            };
            *recorder.lock().expect("lock") = head.clone();
            let body = "1.2.3.4:8080";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    })
    .await;

    let outcome = run_one(lua(
        "ua",
        &format!("return {{ fetch('http://{address}/list') }}"),
    ))
    .await;
    assert!(outcome.ok(), "{:?}", outcome.error);

    let head = seen.lock().expect("lock").clone();
    let user_agent = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        user_agent.contains("mozilla/5.0"),
        "expected a browser UA, got `{head}`"
    );
    assert!(
        !user_agent.contains("proxygate"),
        "our own UA must not be sent any more: {head}"
    );
}

#[tokio::test]
async fn a_script_can_send_its_own_headers() {
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let recorder = seen.clone();
    let address = common::spawn_server(move |mut stream| {
        let recorder = recorder.clone();
        async move {
            use tokio::io::AsyncWriteExt;

            let Some(head) = common::read_head(&mut stream).await else {
                return;
            };
            *recorder.lock().expect("lock") = head.clone();
            let _ = stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
            let _ = stream.flush().await;
        }
    })
    .await;

    let outcome = run_one(lua(
        "headers",
        &format!(
            "return {{ fetch('http://{address}/list', {{ ['Accept-Language'] = 'zh-CN,zh;q=0.9', ['Referer'] = 'http://{address}/' }}) }}"
        ),
    ))
    .await;
    assert!(outcome.ok(), "{:?}", outcome.error);

    let head = seen.lock().expect("lock").clone().to_ascii_lowercase();
    assert!(head.contains("accept-language: zh-cn,zh;q=0.9"), "{head}");
    assert!(
        head.contains(&format!("referer: http://{address}/")),
        "{head}"
    );
}

#[tokio::test]
async fn via_pool_scrapes_through_a_pooled_proxy() {
    // 直连会被 "Blocked"，经过代理才给数据——就像一个按 IP 封的站点。
    let origin = origin_only_behind_proxy("10.0.0.9:8080\n").await;
    let proxy = pretend_proxy("10.0.0.9:8080\n").await;
    let pool = pool_with_proxy(proxy);

    let subscriber = lua(
        "pooled",
        &format!("return {{ fetch('http://{origin}/list') }}"),
    );
    let outcome = run_with_egress(subscriber, EgressPolicy::Pool, Some(pool)).await;

    assert!(outcome.ok(), "{:?}", outcome.error);
    assert_eq!(rendered(&outcome), vec!["http://10.0.0.9:8080".to_string()]);
}

#[tokio::test]
async fn via_direct_never_touches_the_pool() {
    let origin = origin_only_behind_proxy("10.0.0.9:8080\n").await;
    let proxy = pretend_proxy("10.0.0.9:8080\n").await;
    let pool = pool_with_proxy(proxy);

    let subscriber = lua(
        "direct",
        &format!(
            "local ok, body = pcall(fetch, 'http://{origin}/list')\n\
             if ok then return {{ body }} end\n\
             return {{}}"
        ),
    );
    let outcome = run_with_egress(subscriber, EgressPolicy::Direct, Some(pool)).await;

    // 脚本自己 `pcall` 了，所以来源本身成功，但一条代理都没有。
    assert!(outcome.ok(), "{:?}", outcome.error);
    assert_eq!(outcome.count(), 0, "直连被拦，且不该借代理");
}

#[tokio::test]
async fn via_fallback_retries_through_the_pool_after_a_failure() {
    let origin = origin_only_behind_proxy("10.0.0.9:8080\n").await;
    let proxy = pretend_proxy("10.0.0.9:8080\n").await;
    let pool = pool_with_proxy(proxy);

    let subscriber = lua(
        "fallback",
        &format!("return {{ fetch('http://{origin}/list') }}"),
    );
    let outcome = run_with_egress(subscriber, EgressPolicy::Fallback, Some(pool)).await;

    assert!(outcome.ok(), "{:?}", outcome.error);
    assert_eq!(rendered(&outcome), vec!["http://10.0.0.9:8080".to_string()]);
}

#[tokio::test]
async fn an_empty_pool_still_allows_a_direct_fetch() {
    // 冷启动时池子是空的：`via: pool` 不该因此什么都拿不到。
    let address = common::fake_http_server("1.1.1.1:8080\n").await;
    let subscriber = lua(
        "cold",
        &format!("return {{ fetch('http://{address}/list') }}"),
    );
    let outcome = run_with_egress(
        subscriber,
        EgressPolicy::Pool,
        Some(Arc::new(ProxyPool::new())),
    )
    .await;

    assert!(outcome.ok(), "{:?}", outcome.error);
    assert_eq!(rendered(&outcome), vec!["http://1.1.1.1:8080".to_string()]);
}
