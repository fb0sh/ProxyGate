//! Subscriber behaviour: file, http and exec, plus the built-in formats.
//!
//! The theme here is that a subscriber's only job is to produce proxy URLs, and
//! that the `exec` kind is the escape hatch for anything the core does not
//! understand.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use proxygate::config::{Config, Format, SubscriberConfig};
use proxygate::model;
use proxygate::subscriber::SubscriberSet;

/// Creates a scratch directory for a test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("proxygate-it-sub-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn set(config: Config) -> SubscriberSet {
    SubscriberSet::new(&config).expect("subscriber set")
}

fn file_subscriber(name: &str, path: &Path, format: Format) -> SubscriberConfig {
    SubscriberConfig::File {
        name: name.to_string(),
        path: path.to_path_buf(),
        format,
        enabled: true,
    }
}

#[tokio::test]
async fn file_subscriber_reads_a_plaintext_list() {
    let dir = scratch("plain");
    let path = dir.join("proxies.txt");
    std::fs::write(
        &path,
        "# provider list\n\
         http://1.2.3.4:8080\n\
         \n\
         user:pass@2.3.4.5:3128  # fast one\n\
         socks5://5.6.7.8:1080\n\
         this line is not a proxy\n",
    )
    .unwrap();

    let set = set(Config {
        subscribers: vec![file_subscriber("plain", &path, Format::Plaintext)],
        ..Config::default()
    });
    let outcomes = set.fetch_all().await;

    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert!(outcome.ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.count(), 3);

    let rendered: Vec<String> = outcome
        .proxies
        .iter()
        .map(|url| model::render_url(url, true))
        .collect();
    assert_eq!(
        rendered,
        vec![
            "http://1.2.3.4:8080".to_string(),
            "http://user:pass@2.3.4.5:3128".to_string(),
            "socks5://5.6.7.8:1080".to_string(),
        ]
    );
    assert_eq!(outcome.rejected.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn file_subscriber_reads_json_apis() {
    let dir = scratch("json");
    let path = dir.join("proxies.json");
    std::fs::write(
        &path,
        r#"{
            "code": 0,
            "data": [
                {"ip": "1.2.3.4", "port": 8080},
                {"host": "5.6.7.8", "port": 1080, "protocol": "socks5", "username": "u", "password": "p"},
                {"server": "9.9.9.9", "port": 443, "type": "vmess"}
            ]
        }"#,
    )
    .unwrap();

    let set = set(Config {
        subscribers: vec![file_subscriber("json", &path, Format::Json)],
        ..Config::default()
    });
    let outcomes = set.fetch_all().await;
    let outcome = &outcomes[0];
    assert!(outcome.ok(), "unexpected error: {:?}", outcome.error);

    let rendered: Vec<String> = outcome
        .proxies
        .iter()
        .map(|url| model::render_url(url, true))
        .collect();
    assert_eq!(
        rendered,
        vec![
            "http://1.2.3.4:8080".to_string(),
            "socks5://u:p@5.6.7.8:1080".to_string(),
        ]
    );
    assert_eq!(
        outcome.skipped, 1,
        "unsupported protocols are skipped, not rejected"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn file_subscriber_reads_clash_subscriptions() {
    let dir = scratch("clash");
    let path = dir.join("sub.yaml");
    std::fs::write(
        &path,
        "proxies:\n\
         \x20 - name: \"a\"\n\
         \x20   type: http\n\
         \x20   server: 1.2.3.4\n\
         \x20   port: 8080\n\
         \x20 - name: \"b\"\n\
         \x20   type: socks5\n\
         \x20   server: 5.6.7.8\n\
         \x20   port: 1080\n\
         \x20   username: user\n\
         \x20   password: pass\n\
         \x20 - name: \"c\"\n\
         \x20   type: trojan\n\
         \x20   server: 9.9.9.9\n\
         \x20   port: 443\n",
    )
    .unwrap();

    let set = set(Config {
        subscribers: vec![file_subscriber("clash", &path, Format::Clash)],
        ..Config::default()
    });
    let outcomes = set.fetch_all().await;
    let outcome = &outcomes[0];
    assert!(outcome.ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.count(), 2);
    assert_eq!(outcome.skipped, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_subscriber_fetches_over_the_network() {
    let address = common::fake_http_server("1.2.3.4:8080\nuser:pass@5.6.7.8:3128\n").await;

    let set = set(Config {
        subscribers: vec![SubscriberConfig::Http {
            name: "remote".to_string(),
            url: format!("http://{address}/proxies.txt"),
            format: Format::Plaintext,
            headers: BTreeMap::new(),
            timeout: None,
            enabled: true,
        }],
        ..Config::default()
    });

    let outcomes = set.fetch_all().await;
    let outcome = &outcomes[0];
    assert!(outcome.ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.count(), 2);
    assert_eq!(outcome.kind, "http");
}

#[tokio::test]
async fn http_subscriber_reports_a_broken_endpoint() {
    // Nothing listens there.
    let dead = common::dead_address().await;
    let set = set(Config {
        subscribers: vec![SubscriberConfig::Http {
            name: "dead".to_string(),
            url: format!("http://{dead}/proxies.txt"),
            format: Format::Plaintext,
            headers: BTreeMap::new(),
            timeout: Some(std::time::Duration::from_secs(2)),
            enabled: true,
        }],
        ..Config::default()
    });

    let outcomes = set.fetch_all().await;
    assert!(!outcomes[0].ok());
    assert_eq!(outcomes[0].count(), 0);
}

/// The `exec` escape hatch: whatever the provider's format is, a small script
/// turns it into URLs and the core stays simple.
#[cfg(unix)]
#[tokio::test]
async fn exec_subscriber_handles_a_custom_format() {
    let dir = scratch("exec");

    // Pretend this is some provider's unhelpful HTML blob.
    let source = dir.join("provider.html");
    std::fs::write(
        &source,
        "<html><body>\
         <div data-proxy=\"http://1.2.3.4:8080\"></div>\
         <div data-proxy=\"socks5://5.6.7.8:1080\"></div>\
         <span>not a proxy</span>\
         </body></html>",
    )
    .unwrap();

    let script = dir.join("extract.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n# Extract proxy URLs from the provider's HTML.\ngrep -Eo '(http|socks5)://[0-9.:]+' \"$PROVIDER_FILE\" | sort -u\n",
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "PROVIDER_FILE".to_string(),
        source.to_string_lossy().into_owned(),
    );

    let set = set(Config {
        subscribers: vec![SubscriberConfig::Exec {
            name: "custom".to_string(),
            command: vec!["/bin/sh".to_string(), script.to_string_lossy().into_owned()],
            env,
            format: Format::Plaintext,
            timeout: Some(std::time::Duration::from_secs(10)),
            enabled: true,
        }],
        ..Config::default()
    });

    let outcomes = set.fetch_all().await;
    let outcome = &outcomes[0];
    assert!(outcome.ok(), "unexpected error: {:?}", outcome.error);

    let rendered: Vec<String> = outcome
        .proxies
        .iter()
        .map(|url| model::render_url(url, true))
        .collect();
    assert_eq!(
        rendered,
        vec![
            "http://1.2.3.4:8080".to_string(),
            "socks5://5.6.7.8:1080".to_string(),
        ]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn exec_subscriber_can_emit_json() {
    let set = set(Config {
        subscribers: vec![SubscriberConfig::Exec {
            name: "json-exec".to_string(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo '[{\"ip\":\"1.2.3.4\",\"port\":8080},{\"ip\":\"5.6.7.8\",\"port\":1080}]'"
                    .to_string(),
            ],
            env: BTreeMap::new(),
            format: Format::Json,
            timeout: None,
            enabled: true,
        }],
        ..Config::default()
    });

    let outcomes = set.fetch_all().await;
    assert!(
        outcomes[0].ok(),
        "unexpected error: {:?}",
        outcomes[0].error
    );
    assert_eq!(outcomes[0].count(), 2);
}

#[tokio::test]
async fn subscribers_are_fetched_concurrently_and_failures_are_isolated() {
    let dir = scratch("mixed");
    let good = dir.join("good.txt");
    std::fs::write(&good, "1.2.3.4:8080\n").unwrap();

    let dead = common::dead_address().await;

    let set = set(Config {
        subscribers: vec![
            file_subscriber("good", &good, Format::Plaintext),
            SubscriberConfig::Http {
                name: "dead".to_string(),
                url: format!("http://{dead}/x.txt"),
                format: Format::Plaintext,
                headers: BTreeMap::new(),
                timeout: Some(std::time::Duration::from_secs(2)),
                enabled: true,
            },
            SubscriberConfig::File {
                name: "disabled".to_string(),
                path: dir.join("missing.txt"),
                format: Format::Plaintext,
                enabled: false,
            },
        ],
        ..Config::default()
    });

    let outcomes = set.fetch_all().await;
    assert_eq!(outcomes.len(), 2, "disabled subscribers are not fetched");

    let good_outcome = outcomes
        .iter()
        .find(|o| o.name == "good")
        .expect("good outcome");
    let dead_outcome = outcomes
        .iter()
        .find(|o| o.name == "dead")
        .expect("dead outcome");
    assert!(good_outcome.ok());
    assert_eq!(good_outcome.count(), 1);
    assert!(
        !dead_outcome.ok(),
        "a broken subscriber must not hide the working one"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
