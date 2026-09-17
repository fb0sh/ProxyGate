//! Core data model: the normalized [`Proxy`], its stable id, and URL
//! normalization.
//!
//! Everything in ProxyGate is expressed as a `url::Url`. The URL already carries
//! protocol, host, port, username and password, so the model does not duplicate
//! them as separate fields — they are read out on demand.
//!
//! Every proxy that enters the system goes through [`normalize`], which is the
//! single place that decides what a "valid proxy" looks like. Subscribers only
//! have to produce lines; the normalizer turns them into canonical URLs.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};

/// Stable, content-derived identifier of a proxy (16 hex chars).
pub type ProxyId = String;

/// Wire scheme of an upstream proxy.
///
/// v0.1 deliberately supports only what the gateway can actually tunnel
/// through. `https://` (TLS to the upstream proxy itself) is rejected by
/// [`normalize`] instead of silently entering the pool as a proxy that cannot
/// serve CONNECT requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyScheme {
    Http,
    Socks5,
    /// SOCKS5 with remote (proxy side) DNS resolution.
    Socks5h,
}

impl ProxyScheme {
    pub const ALL: [ProxyScheme; 3] =
        [ProxyScheme::Http, ProxyScheme::Socks5, ProxyScheme::Socks5h];

    pub const fn as_str(self) -> &'static str {
        match self {
            ProxyScheme::Http => "http",
            ProxyScheme::Socks5 => "socks5",
            ProxyScheme::Socks5h => "socks5h",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            ProxyScheme::Http => 80,
            ProxyScheme::Socks5 | ProxyScheme::Socks5h => 1080,
        }
    }

    pub const fn is_socks(self) -> bool {
        matches!(self, ProxyScheme::Socks5 | ProxyScheme::Socks5h)
    }

    /// Parses a URL scheme, accepting the common aliases.
    pub fn parse(scheme: &str) -> Option<Self> {
        match scheme.to_ascii_lowercase().as_str() {
            "http" => Some(ProxyScheme::Http),
            "socks" | "socks5" => Some(ProxyScheme::Socks5),
            "socks5h" => Some(ProxyScheme::Socks5h),
            _ => None,
        }
    }
}

/// Outcome of probing one health target through one proxy.
///
/// The target is shared between every proxy (`Arc<str>`), so keeping the full
/// per-target result on each proxy stays cheap to clone.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub target: Arc<str>,
    pub ok: bool,
    pub latency: Option<Duration>,
}

impl ProbeOutcome {
    /// The target without its scheme, for compact one-line output.
    pub fn label(&self) -> &str {
        self.target
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.target)
    }
}

/// A single upstream proxy and what we currently know about it.
///
/// `alive`, `latency`, `failures` and `probes` are runtime facts and are never
/// persisted; `generation` and `last_used_at` are usage facts and *are*
/// persisted so that repeated CLI invocations keep rotating through the pool.
#[derive(Debug, Clone)]
pub struct Proxy {
    pub id: ProxyId,
    pub url: Url,

    pub alive: bool,
    pub latency: Option<Duration>,
    pub failures: u32,

    /// Result of the last health pass, one entry per configured target.
    pub probes: Vec<ProbeOutcome>,

    pub last_checked_at: Option<SystemTime>,
    pub last_used_at: Option<SystemTime>,
    pub generation: u64,
}

impl Proxy {
    /// Builds a proxy from an already-normalized URL.
    pub fn new(url: Url) -> Self {
        let id = Self::id_of(&url);
        Self {
            id,
            url,
            alive: false,
            latency: None,
            failures: 0,
            probes: Vec::new(),
            last_checked_at: None,
            last_used_at: None,
            generation: 0,
        }
    }

    /// Stable id derived from the canonical rendering of the URL.
    pub fn id_of(url: &Url) -> ProxyId {
        format!("{:016x}", fnv1a64(render_url(url, true).as_bytes()))
    }

    pub fn scheme(&self) -> ProxyScheme {
        ProxyScheme::parse(self.url.scheme()).unwrap_or(ProxyScheme::Http)
    }

    pub fn host(&self) -> &str {
        self.url.host_str().unwrap_or_default()
    }

    /// Explicit port, or the scheme default when the URL omits it.
    pub fn port(&self) -> u16 {
        self.url
            .port()
            .unwrap_or_else(|| self.scheme().default_port())
    }

    /// `host:port`, the form the gateway needs for CONNECT and for connecting
    /// to the upstream itself.
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host(), self.port())
    }

    /// Percent-decoded username, if the URL carries one.
    pub fn username(&self) -> Option<String> {
        if self.url.username().is_empty() {
            None
        } else {
            Some(percent_decode(self.url.username()))
        }
    }

    /// Percent-decoded password, if the URL carries one.
    pub fn password(&self) -> Option<String> {
        self.url.password().map(percent_decode)
    }

    pub fn has_auth(&self) -> bool {
        !self.url.username().is_empty() || self.url.password().is_some()
    }

    pub fn latency_ms(&self) -> Option<u64> {
        self.latency.map(|d| d.as_millis() as u64)
    }

    /// How many of the probed targets answered for this proxy.
    pub fn targets_passed(&self) -> usize {
        self.probes.iter().filter(|probe| probe.ok).count()
    }

    /// Compact `passed/total` summary, or `-` when the proxy was never probed.
    pub fn targets_summary(&self) -> String {
        if self.probes.is_empty() {
            "-".to_string()
        } else {
            format!("{}/{}", self.targets_passed(), self.probes.len())
        }
    }

    /// Targets that failed the last pass, for diagnostics.
    pub fn failed_targets(&self) -> Vec<&str> {
        self.probes
            .iter()
            .filter(|probe| !probe.ok)
            .map(|probe| probe.target.as_ref())
            .collect()
    }

    /// Full URL including credentials — this is what `proxygate get` prints.
    pub fn to_full_string(&self) -> String {
        render_url(&self.url, true)
    }

    /// URL with credentials replaced by `***:***`.
    pub fn to_masked_string(&self) -> String {
        render_url(&self.url, false)
    }

    pub fn render(&self, show_auth: bool) -> String {
        render_url(&self.url, show_auth)
    }

    /// Human readable status used by `proxygate list`.
    pub fn status(&self) -> &'static str {
        if self.alive { "alive" } else { "dead" }
    }
}

/// Canonical rendering of a proxy URL: always explicit port, no path/query,
/// IPv6 hosts bracketed.
pub fn render_url(url: &Url, show_auth: bool) -> String {
    let scheme = ProxyScheme::parse(url.scheme()).unwrap_or(ProxyScheme::Http);
    let host = url.host_str().unwrap_or_default();
    let port = url.port().unwrap_or_else(|| scheme.default_port());

    let mut out = String::with_capacity(48);
    let _ = write!(out, "{}://", scheme.as_str());
    if !url.username().is_empty() || url.password().is_some() {
        if show_auth {
            out.push_str(url.username());
            if let Some(password) = url.password() {
                out.push(':');
                out.push_str(password);
            }
        } else {
            out.push_str("***:***");
        }
        out.push('@');
    }
    let _ = write!(out, "{host}:{port}");
    out
}

/// Turns one raw subscriber line into a canonical proxy URL.
///
/// Accepted inputs:
///
/// ```text
/// 1.2.3.4:8080
/// user:pass@1.2.3.4:8080
/// http://1.2.3.4:8080
/// http://user:pass@1.2.3.4:8080
/// socks5://1.2.3.4:1080
/// socks5h://user:pass@[2001:db8::1]:1080
/// ```
///
/// Anything else is rejected with a reason. `https://`, `socks4://` and other
/// schemes are rejected on purpose: the gateway cannot tunnel through them.
pub fn normalize(input: &str) -> Result<Url> {
    let cleaned = clean_line(input);
    if cleaned.is_empty() {
        return Err(Error::invalid(input, "empty line"));
    }

    // A bare `host:port` (with or without credentials) is the common case; assume
    // plain HTTP, the scheme used by the overwhelming majority of proxy lists.
    let candidate = if cleaned.contains("://") {
        cleaned.to_string()
    } else {
        format!("http://{cleaned}")
    };

    let parsed = Url::parse(&candidate).map_err(|e| Error::invalid(input, e.to_string()))?;
    let mut parsed = parsed;

    let scheme = ProxyScheme::parse(parsed.scheme()).ok_or_else(|| {
        Error::invalid(input, format!("unsupported scheme `{}`", parsed.scheme()))
    })?;

    if parsed.host_str().map(str::is_empty).unwrap_or(true) {
        return Err(Error::invalid(input, "missing host"));
    }

    if let Some(port) = parsed.port() {
        if port == 0 {
            return Err(Error::invalid(input, "port 0 is not usable"));
        }
    }

    // Canonicalize in place: never re-serialize the userinfo by hand, or
    // percent-escapes would be encoded twice.
    if parsed.scheme() != scheme.as_str() {
        parsed
            .set_scheme(scheme.as_str())
            .map_err(|_| Error::invalid(input, "unsupported scheme"))?;
    }
    parsed.set_path("");
    parsed.set_query(None);
    parsed.set_fragment(None);

    Ok(parsed)
}

/// Trims a raw line: surrounding whitespace, a UTF-8 BOM and a trailing CR.
pub fn clean_line(input: &str) -> &str {
    input.trim().trim_start_matches('\u{feff}').trim()
}

/// True for lines that a plaintext list uses as comments or padding.
pub fn is_ignorable_line(line: &str) -> bool {
    let cleaned = clean_line(line);
    cleaned.is_empty() || cleaned.starts_with('#') || cleaned.starts_with("//")
}

/// Strips an inline `# comment` when it is preceded by whitespace.
///
/// A `#` inside credentials or a host is left alone.
pub fn strip_inline_comment(line: &str) -> &str {
    let cleaned = clean_line(line);
    for (idx, ch) in cleaned.char_indices() {
        if ch == '#' && idx > 0 && cleaned[..idx].ends_with(char::is_whitespace) {
            return cleaned[..idx].trim_end();
        }
    }
    cleaned
}

/// FNV-1a 64 bit — small, stable and dependency free.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Percent-decodes a URL component (invalid escapes are passed through).
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
///
/// Implemented here rather than pulled in as a dependency: the gateway needs it
/// for exactly one thing (the `Proxy-Authorization: Basic` header).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(B64_ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decodes standard base64; whitespace is ignored, bad input yields `None`.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_bare_host_port() {
        let url = normalize("1.2.3.4:8080").unwrap();
        assert_eq!(render_url(&url, true), "http://1.2.3.4:8080");
    }

    #[test]
    fn normalizes_credentials_and_scheme_aliases() {
        let url = normalize("socks://foo:bar@5.6.7.8:1080").unwrap();
        assert_eq!(render_url(&url, true), "socks5://foo:bar@5.6.7.8:1080");

        let url = normalize("user:pass@1.2.3.4:3128").unwrap();
        assert_eq!(render_url(&url, true), "http://user:pass@1.2.3.4:3128");
        assert_eq!(url.username(), "user");
        assert_eq!(url.password(), Some("pass"));
    }

    #[test]
    fn applies_scheme_default_ports() {
        assert_eq!(
            render_url(&normalize("1.2.3.4").unwrap(), true),
            "http://1.2.3.4:80"
        );
        assert_eq!(
            render_url(&normalize("socks5://1.2.3.4").unwrap(), true),
            "socks5://1.2.3.4:1080"
        );
    }

    #[test]
    fn strips_path_query_and_bom() {
        let url = normalize("\u{feff}http://1.2.3.4:8080/some/path?x=1\r").unwrap();
        assert_eq!(render_url(&url, true), "http://1.2.3.4:8080");
    }

    #[test]
    fn keeps_ipv6_hosts_bracketed() {
        let url = normalize("socks5h://user:pw@[2001:db8::1]:1080").unwrap();
        assert_eq!(
            render_url(&url, true),
            "socks5h://user:pw@[2001:db8::1]:1080"
        );
    }

    #[test]
    fn rejects_unsupported_schemes_and_garbage() {
        assert!(normalize("https://1.2.3.4:8080").is_err());
        assert!(normalize("socks4://1.2.3.4:1080").is_err());
        assert!(normalize("").is_err());
        assert!(normalize("http://").is_err());
        assert!(normalize("http://1.2.3.4:0").is_err());
    }

    #[test]
    fn id_is_stable_across_equivalent_spellings() {
        let a = Proxy::new(normalize("http://1.2.3.4:8080").unwrap());
        let b = Proxy::new(normalize("1.2.3.4:8080").unwrap());
        let c = Proxy::new(normalize("HTTP://1.2.3.4:8080/").unwrap());
        assert_eq!(a.id, b.id);
        assert_eq!(b.id, c.id);

        let d = Proxy::new(normalize("1.2.3.4:8081").unwrap());
        assert_ne!(a.id, d.id);
    }

    #[test]
    fn masks_credentials() {
        let proxy = Proxy::new(normalize("http://user:pass@1.2.3.4:3128").unwrap());
        assert_eq!(proxy.to_masked_string(), "http://***:***@1.2.3.4:3128");
        assert_eq!(proxy.to_full_string(), "http://user:pass@1.2.3.4:3128");
        assert_eq!(proxy.authority(), "1.2.3.4:3128");
        assert!(proxy.has_auth());
    }

    #[test]
    fn base64_round_trips() {
        for input in ["", "a", "ab", "abc", "admin:secret", "\u{1f600} bytes"] {
            let encoded = base64_encode(input.as_bytes());
            assert_eq!(base64_decode(&encoded).unwrap(), input.as_bytes());
        }
        assert_eq!(base64_encode(b"admin:secret"), "YWRtaW46c2VjcmV0");
        assert_eq!(base64_decode("YWRtaW46c2VjcmV0").unwrap(), b"admin:secret");
        assert_eq!(base64_decode("!!!").unwrap_or_default(), Vec::<u8>::new());
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("p%40ss%3Aword"), "p@ss:word");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn never_panics_on_hostile_input() {
        // A provider list is untrusted input: garbage must be rejected, never
        // panic, because `refresh` feeds every line through here.
        let long = "a".repeat(4096);
        let inputs = [
            "",
            " ",
            "\t\r\n",
            "#",
            "# comment",
            "://",
            "http://",
            "http://:",
            "http://:8080",
            "http://[]",
            "http://[::1",
            "http://[::1]:",
            "http://1.2.3.4:99999",
            "http://1.2.3.4:-1",
            "http://1.2.3.4:0",
            "user:pass@@1.2.3.4:80",
            "user:@1.2.3.4:80",
            "@1.2.3.4:80",
            "socks5://%zz:%%@host:1080",
            "http://1.2.3.4:8080/../../etc/passwd?a=1#frag",
            "HTTP://EXAMPLE.COM:80",
            "http://\u{1f600}:\u{1f4a9}@1.2.3.4:80",
            "\u{feff}\u{200b}http://1.2.3.4:80",
            "http://exa mple.com:80",
            "\0\0\0",
            "-",
            ":",
            ":::",
            "localhost",
            "localhost:",
            "localhost:notaport",
            long.as_str(),
            "http://1.2.3.4:8080\u{0}",
            "socks4://1.2.3.4:1080",
            "https://1.2.3.4:443",
            "ftp://1.2.3.4:21",
        ];

        for input in inputs {
            // The only contract is "returns", and when it succeeds the URL must
            // be renderable and id-able without panicking.
            if let Ok(url) = normalize(input) {
                let _ = render_url(&url, true);
                let _ = render_url(&url, false);
                let proxy = Proxy::new(url);
                assert!(!proxy.id.is_empty());
                assert!(!proxy.authority().is_empty());
                let _ = proxy.to_full_string();
                let _ = proxy.to_masked_string();
            }
        }
    }

    #[test]
    fn decodes_hostile_base64() {
        for input in [
            "",
            "=",
            "====",
            "a",
            "!!!!",
            "\u{1f600}",
            "YQ",
            "9",
            "////",
            "AAAA\n",
        ] {
            let _ = base64_decode(input);
        }
        for input in ["", "%", "%2", "%zz", "%41", "%%41", "\u{1f600}"] {
            let _ = percent_decode(input);
        }
    }

    #[test]
    fn detects_ignorable_lines() {
        assert!(is_ignorable_line(""));
        assert!(is_ignorable_line("   "));
        assert!(is_ignorable_line("# comment"));
        assert!(!is_ignorable_line("1.2.3.4:8080"));
        assert_eq!(strip_inline_comment("1.2.3.4:8080  # fast"), "1.2.3.4:8080");
        assert_eq!(
            strip_inline_comment("http://u:p#x@1.2.3.4:80"),
            "http://u:p#x@1.2.3.4:80"
        );
    }
}
