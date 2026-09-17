//! 调试工具：把一份真实载荷喂给真正的解析器，打印各协议的条数。
//!
//! 用来核对「某个来源到底解析出了什么」，不要在 CI 里跑（需要真实载荷）：
//!
//! ```console
//! cargo run --example parse_probe -- /tmp/pg-verify/*.json
//! ```

use std::collections::BTreeMap;

use proxygate::config::Format;
use proxygate::model::normalize;
use proxygate::subscriber::parse_payload;

fn main() {
    for path in std::env::args().skip(1) {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                println!("{path}: cannot read: {error}");
                continue;
            }
        };

        let parsed = match parse_payload(&text, Format::Json) {
            Ok(parsed) => parsed,
            Err(error) => {
                println!("{path}: parse error: {error}");
                continue;
            }
        };

        let mut schemes: BTreeMap<String, usize> = BTreeMap::new();
        let mut rejected = 0;
        for candidate in &parsed.candidates {
            match normalize(candidate) {
                Ok(url) => *schemes.entry(url.scheme().to_string()).or_default() += 1,
                Err(_) => rejected += 1,
            }
        }

        let name = path.rsplit('/').next().unwrap_or(&path);
        println!(
            "{name:16} candidates={:5} skipped={:4} rejected={} {schemes:?}",
            parsed.candidates.len(),
            parsed.skipped,
            rejected
        );
    }
}
