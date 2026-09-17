//! 核心数据模型：归一化后的 [`Proxy`]、其稳定标识符，
//! 以及 URL 归一化。
//!
//! ProxyGate 中的一切都用 `url::Url` 表示。
//! URL 本身就携带协议、主机、端口、用户名和密码，
//! 因此模型不再把它们复制成独立字段，而是在需要时按需读取。
//!
//! 每个进入系统的代理都要经过 [`normalize`]，
//! 它单独决定了“合法代理”应该是什么样子。
//! 订阅源只需产出文本行，
//! 归一化会把这些文本行转换成规范 URL。

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};

/// 代理的稳定标识符，由内容派生（16 个十六进制字符）。
pub type ProxyId = String;

/// 上游代理的线路协议。
///
/// v0.1 只支持网关真正能隧穿的协议。
/// `https://`（到上游代理本身的 TLS）
/// 会被 [`normalize`] 拒绝，而不是悄悄进入代理池、
/// 变成一个无法处理 CONNECT 请求的代理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyScheme {
    /// 明文 HTTP 代理。
    Http,
    /// 在本地（客户端侧）解析 DNS 的 SOCKS5。
    Socks5,
    /// 在远端（代理侧）解析 DNS 的 SOCKS5。
    Socks5h,
}

impl ProxyScheme {
    /// 所有支持的代理协议。
    pub const ALL: [ProxyScheme; 3] =
        [ProxyScheme::Http, ProxyScheme::Socks5, ProxyScheme::Socks5h];

    /// 协议的规范小写名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            ProxyScheme::Http => "http",
            ProxyScheme::Socks5 => "socks5",
            ProxyScheme::Socks5h => "socks5h",
        }
    }

    /// URL 省略端口时使用的协议默认端口。
    pub const fn default_port(self) -> u16 {
        match self {
            ProxyScheme::Http => 80,
            ProxyScheme::Socks5 | ProxyScheme::Socks5h => 1080,
        }
    }

    /// 该协议是否属于 SOCKS5 家族。
    pub const fn is_socks(self) -> bool {
        matches!(self, ProxyScheme::Socks5 | ProxyScheme::Socks5h)
    }

    /// 解析 URL 协议名，并接受常见别名。
    pub fn parse(scheme: &str) -> Option<Self> {
        match scheme.to_ascii_lowercase().as_str() {
            "http" => Some(ProxyScheme::Http),
            "socks" | "socks5" => Some(ProxyScheme::Socks5),
            "socks5h" => Some(ProxyScheme::Socks5h),
            _ => None,
        }
    }
}

/// 通过一个代理探测一个健康检查目标的结果。
///
/// 目标在所有代理之间共享（`Arc<str>`），
/// 因此在每个代理上保留完整的逐目标结果仍然可以廉价地克隆。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// 被探测的目标 URL。
    pub target: Arc<str>,
    /// 该目标是否探测成功。
    pub ok: bool,
    /// 本次探测的耗时，未测量时为 `None`。
    pub latency: Option<Duration>,
}

impl ProbeOutcome {
    /// 去掉协议前缀的目标，用于紧凑的单行输出。
    pub fn label(&self) -> &str {
        self.target
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.target)
    }
}

/// 单个上游代理，以及当前已知的相关信息。
///
/// `alive`、`latency`、`failures` 和 `probes` 是运行时事实，
/// 永不持久化；`generation` 和 `last_used_at` 是使用事实，
/// 会被持久化，好让多次 CLI 调用之间继续轮换整个代理池。
#[derive(Debug, Clone)]
pub struct Proxy {
    /// 由规范化 URL 派生的稳定标识符。
    pub id: ProxyId,
    /// 归一化后的代理 URL，其中可能包含凭据。
    pub url: Url,

    /// 最近一次健康检查是否成功。
    pub alive: bool,
    /// 最近一次测量到的延迟。
    pub latency: Option<Duration>,
    /// 连续失败次数。
    pub failures: u32,

    /// 最近一轮健康检查的结果，每个已配置目标一个条目。
    pub probes: Vec<ProbeOutcome>,

    /// 最近一次健康检查的时间；从未检查时为 `None`。
    pub last_checked_at: Option<SystemTime>,
    /// 最近一次被选中的时间；从未使用时为 `None`。
    pub last_used_at: Option<SystemTime>,
    /// 该代理最后一次被使用的轮次。
    pub generation: u64,
}

impl Proxy {
    /// 基于已经归一化的 URL 构造一个代理。
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

    /// 由 URL 的规范渲染形式派生的稳定标识符。
    pub fn id_of(url: &Url) -> ProxyId {
        format!("{:016x}", fnv1a64(render_url(url, true).as_bytes()))
    }

    /// 该代理的协议。
    pub fn scheme(&self) -> ProxyScheme {
        ProxyScheme::parse(self.url.scheme()).unwrap_or(ProxyScheme::Http)
    }

    /// 该代理的主机名（不含端口）。
    pub fn host(&self) -> &str {
        self.url.host_str().unwrap_or_default()
    }

    /// 显式端口；URL 省略端口时使用协议默认端口。
    pub fn port(&self) -> u16 {
        self.url
            .port()
            .unwrap_or_else(|| self.scheme().default_port())
    }

    /// `host:port` 形式，
    /// 网关处理 CONNECT 以及连接上游本身时都需要它。
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host(), self.port())
    }

    /// 百分号解码后的用户名；URL 未携带时为 `None`。
    pub fn username(&self) -> Option<String> {
        if self.url.username().is_empty() {
            None
        } else {
            Some(percent_decode(self.url.username()))
        }
    }

    /// 百分号解码后的密码；URL 未携带时为 `None`。
    pub fn password(&self) -> Option<String> {
        self.url.password().map(percent_decode)
    }

    /// URL 是否携带用户名或密码。
    pub fn has_auth(&self) -> bool {
        !self.url.username().is_empty() || self.url.password().is_some()
    }

    /// 延迟的毫秒数；未测量时为 `None`。
    pub fn latency_ms(&self) -> Option<u64> {
        self.latency.map(|d| d.as_millis() as u64)
    }

    /// 该代理成功响应的探测目标数量。
    pub fn targets_passed(&self) -> usize {
        self.probes.iter().filter(|probe| probe.ok).count()
    }

    /// 紧凑的 `passed/total` 摘要；从未探测过时返回 `-`。
    pub fn targets_summary(&self) -> String {
        if self.probes.is_empty() {
            "-".to_string()
        } else {
            format!("{}/{}", self.targets_passed(), self.probes.len())
        }
    }

    /// 最近一轮探测失败的目标，用于诊断。
    pub fn failed_targets(&self) -> Vec<&str> {
        self.probes
            .iter()
            .filter(|probe| !probe.ok)
            .map(|probe| probe.target.as_ref())
            .collect()
    }

    /// 包含凭据的完整 URL，即 `proxygate get` 打印的内容。
    pub fn to_full_string(&self) -> String {
        render_url(&self.url, true)
    }

    /// 脱敏后的 URL，凭据被替换为 `***:***`。
    pub fn to_masked_string(&self) -> String {
        render_url(&self.url, false)
    }

    /// 按 `show_auth` 决定是否显示凭据来渲染 URL。
    pub fn render(&self, show_auth: bool) -> String {
        render_url(&self.url, show_auth)
    }

    /// `proxygate list` 使用的人类可读状态。
    pub fn status(&self) -> &'static str {
        if self.alive { "alive" } else { "dead" }
    }
}

/// 代理 URL 的规范渲染形式：总是显式端口、不带路径和查询、
/// IPv6 主机加方括号。
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

/// 把订阅源的一行原始文本转换成规范的代理 URL。
///
/// 可接受的输入：
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
/// 其他输入都会被拒绝并给出原因。
/// `https://`、`socks4://` 以及其他协议被有意拒绝：
/// 网关无法通过它们建立隧道。
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

/// 清理一行原始文本：首尾空白、UTF-8 BOM 和结尾的 CR。
pub fn clean_line(input: &str) -> &str {
    input.trim().trim_start_matches('\u{feff}').trim()
}

/// 判断一行是否属于纯文本列表中的注释或填充行。
pub fn is_ignorable_line(line: &str) -> bool {
    let cleaned = clean_line(line);
    cleaned.is_empty() || cleaned.starts_with('#') || cleaned.starts_with("//")
}

/// 去掉前面带空白的行内 `# 注释`。
///
/// 凭据或主机名内部的 `#` 保持原样。
pub fn strip_inline_comment(line: &str) -> &str {
    let cleaned = clean_line(line);
    for (idx, ch) in cleaned.char_indices() {
        if ch == '#' && idx > 0 && cleaned[..idx].ends_with(char::is_whitespace) {
            return cleaned[..idx].trim_end();
        }
    }
    cleaned
}

/// FNV-1a 64 位哈希：体积小、结果稳定且无外部依赖。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 把单个十六进制字符转换为对应的数值。
fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 对 URL 组件做百分号解码（非法转义按原样保留）。
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

/// 标准 base64 字母表。
const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 带填充的标准 base64。
///
/// 在这里自行实现而不引入依赖：
/// 网关只在一处需要它，即 `Proxy-Authorization: Basic` 请求头。
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

/// 解码标准 base64；空白会被忽略，输入非法时返回 `None`。
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
