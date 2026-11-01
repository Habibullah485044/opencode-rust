//! The `webfetch` tool: S-06 checked fetches (public addresses only, every
//! redirect checked, capped bodies), web search and HTML to text.

use super::truncate;
use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;

/// S-06: true only for normal public internet addresses. Loopback, private,
/// link-local (cloud metadata), shared/CGNAT, multicast, broadcast,
/// documentation, benchmarking, reserved and unspecified addresses are not.
/// IPv6 forms that wrap an IPv4 address are judged by that IPv4 address.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d and ::a.b.c.d
            if let Some(v4) = v6.to_ipv4() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            let embedded =
                |hi: u16, lo: u16| IpAddr::V4(Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8));
            // 64:ff9b::/96 (NAT64) and 2002::/16 (6to4) carry an IPv4 address.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                return is_public_ip(embedded(s[6], s[7]));
            }
            if s[0] == 0x2002 {
                return is_public_ip(embedded(s[1], s[2]));
            }
            // 2001::/32 (Teredo) tunnels to an IPv4 address hidden in the
            // address (XOR-ed); no public page needs it, so it is refused.
            if s[0] == 0x2001 && s[1] == 0 {
                return false;
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xffc0) == 0xfec0
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

/// webfetch reads at most this many bytes of a response body (S-06).
const WEBFETCH_MAX_BYTES: usize = 2_000_000;
/// webfetch follows at most this many redirects (S-06).
const WEBFETCH_MAX_REDIRECTS: usize = 5;

pub(super) async fn webfetch(url: &str) -> Result<String> {
    fetch_checked(url, is_public_ip).await
}

/// S-06: GET `url` from addresses that pass `allowed` only. Checks every
/// hop, pins each request to the checked addresses, caps the body.
/// Returns the body and whether it was cut.
async fn fetch_body(url: &str, allowed: fn(std::net::IpAddr) -> bool) -> Result<(Vec<u8>, bool)> {
    let mut current = reqwest::Url::parse(url).with_context(|| format!("invalid url `{url}`"))?;
    for _ in 0..=WEBFETCH_MAX_REDIRECTS {
        if !matches!(current.scheme(), "http" | "https") {
            bail!("only http(s) urls are allowed");
        }
        let host = current
            .host_str()
            .ok_or_else(|| anyhow!("url `{current}` has no host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = current.port_or_known_default().unwrap_or(80);
        let addrs: Vec<std::net::SocketAddr> = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host.as_str(), port))
                .await
                .with_context(|| format!("cannot resolve {host}"))?
                .collect(),
        };
        if addrs.is_empty() {
            bail!("{host} did not resolve to any address");
        }
        if let Some(bad) = addrs.iter().find(|a| !allowed(a.ip())) {
            bail!(
                "refused: {host} points to {}, a local or private network address. \
                 webfetch only reads public internet pages.",
                bad.ip()
            );
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("opencode-rust/0.1")
            .redirect(reqwest::redirect::Policy::none())
            // An HTTP(S)_PROXY from the environment would connect to the
            // proxy instead of the checked addresses (and the proxy can
            // reach the private network): always connect directly.
            .no_proxy()
            .resolve_to_addrs(&host, &addrs)
            .build()?;
        let mut resp = http.get(current.clone()).send().await.with_context(|| format!("fetching {current}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let next = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("{current} returned {status} without a Location header"))?;
            current = current.join(next).with_context(|| format!("bad redirect target `{next}`"))?;
            continue;
        }
        if !status.is_success() {
            bail!("{current} returned {status}");
        }
        let mut body: Vec<u8> = Vec::new();
        let mut cut = false;
        while let Some(chunk) = resp.chunk().await? {
            let room = WEBFETCH_MAX_BYTES - body.len();
            if chunk.len() >= room {
                body.extend_from_slice(&chunk[..room]);
                cut = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        return Ok((body, cut));
    }
    bail!("too many redirects (more than {WEBFETCH_MAX_REDIRECTS}) starting at {url}")
}

/// S-06 fetch as page text (html stripped, capped).
async fn fetch_checked(url: &str, allowed: fn(std::net::IpAddr) -> bool) -> Result<String> {
    let (body, cut) = fetch_body(url, allowed).await?;
    let mut text = truncate(&html_to_text(&String::from_utf8_lossy(&body)), 20_000);
    if cut {
        text.push_str(&format!("\n… [page is larger than {WEBFETCH_MAX_BYTES} bytes; only the start was read]"));
    }
    Ok(text)
}

/// Web search without an API key: DuckDuckGo's HTML results page, read
/// through the same checked fetch as webfetch. Up to 8 results.
pub(super) async fn web_search(query: &str) -> Result<String> {
    let mut url = reqwest::Url::parse("https://html.duckduckgo.com/html/")?;
    url.query_pairs_mut().append_pair("q", query);
    let (body, _) = fetch_body(url.as_str(), is_public_ip).await?;
    let results = parse_search_results(&String::from_utf8_lossy(&body), 8);
    if results.is_empty() {
        return Ok(format!("no results for \"{query}\""));
    }
    Ok(results
        .iter()
        .enumerate()
        .map(|(i, (title, link, snippet))| format!("{}. {title}\n   {link}\n   {snippet}", i + 1))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// (title, url, snippet) from a DuckDuckGo HTML results page.
fn parse_search_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for block in html.split("class=\"result__a\"").skip(1) {
        if out.len() >= max {
            break;
        }
        let href = block.split("href=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
        let href = href.replace("&amp;", "&");
        // Result links go through a redirect: the target is in `uddg`.
        let link = reqwest::Url::parse(&format!("https:{}", href.trim_start_matches("https:")))
            .ok()
            .and_then(|u| u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned()))
            .unwrap_or(href);
        let title = block.split_once('>').and_then(|(_, s)| s.split("</a>").next()).unwrap_or("");
        let snippet = block
            .split("class=\"result__snippet\"")
            .nth(1)
            .and_then(|s| s.split_once('>'))
            .and_then(|(_, s)| s.split("</a>").next())
            .unwrap_or("");
        // Inline tags (<b> around the matched words) go without a space.
        let clean = |s: &str| {
            let mut plain = String::new();
            let mut in_tag = false;
            for c in s.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => plain.push(c),
                    _ => {}
                }
            }
            html_to_text(&plain).split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let (title, snippet) = (clean(title), clean(snippet));
        if !title.is_empty() && link.starts_with("http") && !out.iter().any(|(_, l, _)| *l == link) {
            out.push((title, link, snippet.chars().take(240).collect()));
        }
    }
    out
}

/// Very small HTML→text: drop script/style, collapse tags to spaces, unescape basics.
pub(super) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < html.len() {
        if lower[i..].starts_with("<script") || lower[i..].starts_with("<style") {
            // skip to closing tag
            let close = lower[i..].find('>').map(|p| i + p + 1).unwrap_or(html.len());
            let end_tag = lower[close..].find("</script>").or_else(|| lower[close..].find("</style>"));
            i = match end_tag {
                Some(p) => close + p + if lower[close + p..].starts_with("</script") { 9 } else { 8 },
                None => html.len(),
            };
            continue;
        }
        if bytes[i] == b'<' {
            match html[i..].find('>') {
                Some(p) => {
                    out.push(' ');
                    i += p + 1;
                    continue;
                }
                // B-43: no `>` anywhere after this point, so no more tags.
                None => {
                    out.push_str(&html[i..]);
                    break;
                }
            }
        }
        let ch_len = utf8_len(bytes[i]);
        let end = (i + ch_len).min(html.len());
        out.push_str(&html[i..end]);
        i = end;
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        // B-43: `&amp;` last, so `&amp;lt;` stays the text `&lt;`.
        .replace("&amp;", "&")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

#[cfg(test)]
mod s06_ip_tests {
    use super::*;

    #[test]
    fn only_public_addresses_pass() {
        let ip = |s: &str| -> std::net::IpAddr { s.parse().unwrap() };
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2002:c0a8:101::1",
        ] {
            assert!(!is_public_ip(ip(bad)), "{bad} must be refused");
        }
        for good in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "64:ff9b::808:808"] {
            assert!(is_public_ip(ip(good)), "{good} must be allowed");
        }
    }
}

#[cfg(test)]
mod s06_fetch_tests {
    use super::*;
    use axum::http::{header, StatusCode};
    use axum::{routing::get, Router};

    fn loopback_only(ip: std::net::IpAddr) -> bool {
        ip.is_loopback()
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn webfetch_refuses_local_hosts_and_parses_search_results() {
        let page = r#"<div class="result"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&amp;rut=x">Tokio <b>docs</b></a>
<a class="result__snippet" href="x">An async runtime for <b>Rust</b>.</a></div>
<div class="result"><a class="result__a" href="https://example.com/b">Second</a></div>"#;
        let r = parse_search_results(page, 8);
        assert_eq!(r[0], ("Tokio docs".into(), "https://docs.rs/tokio".into(), "An async runtime for Rust.".into()));
        assert_eq!(r[1].1, "https://example.com/b");
        assert_eq!(parse_search_results(page, 1).len(), 1);
        for url in ["http://127.0.0.1:9/", "http://localhost:9/", "http://[::1]:9/", "http://10.0.0.1/"] {
            let err = webfetch(url).await.unwrap_err().to_string();
            assert!(err.contains("refused"), "{url}: {err}");
        }
    }

    #[tokio::test]
    async fn every_redirect_is_checked_and_loops_stop() {
        let app = Router::new()
            .route("/start", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "http://10.255.255.1/x")], "") }))
            .route("/loop", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop")], "") }));
        let base = serve(app).await;
        let err = fetch_checked(&format!("{base}/start"), loopback_only).await.unwrap_err().to_string();
        assert!(err.contains("10.255.255.1"), "{err}");
        let err = fetch_checked(&format!("{base}/loop"), loopback_only).await.unwrap_err().to_string();
        assert!(err.contains("too many redirects"), "{err}");
    }

    #[tokio::test]
    async fn big_bodies_are_cut() {
        let app = Router::new().route("/big", get(|| async { "a".repeat(3_000_000) }));
        let base = serve(app).await;
        let out = fetch_checked(&format!("{base}/big"), loopback_only).await.unwrap();
        assert!(out.contains("only the start was read"), "size note missing");
    }
}

#[cfg(test)]
mod b43_tests {
    use super::*;

    #[test]
    fn entities_are_decoded_once_and_stray_brackets_are_fast() {
        assert_eq!(html_to_text("<p>&amp;lt;b&amp;gt; &lt;i&gt;</p>"), "&lt;b&gt; <i>");
        let started = std::time::Instant::now();
        let text = html_to_text(&"a<".repeat(100_000));
        assert_eq!(text.len(), 200_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
