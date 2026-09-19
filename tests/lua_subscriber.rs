//! Lua subscribers, driven over a real socket.
//!
//! The theme here is that a subscriber is a script: it fetches what it wants,
//! reshapes it into proxy tables, and returns them — while staying inside a
//! sandbox whose only way out is `fetch`.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use proxygate::config::{Config, SubscriberConfig};
use proxygate::subscriber::SubscriberSet;

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
