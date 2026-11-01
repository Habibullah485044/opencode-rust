//! The preview panel's URL rules (S-11): which pages it may open, and how a
//! typed address becomes a URL.

use super::*;

/// New (S-11): the one rule for every page the preview panel opens — typed
/// URLs, the agent's `browser_navigate`, the dev-server button, links,
/// redirects and back/forward. `Err` carries the message the panel shows.
pub fn check_navigation(url: &str, allow_remote: bool) -> Result<(), String> {
    if !is_allowed_scheme(url) {
        return Err(format!("Blocked: `{url}` — the panel only loads http(s) pages."));
    }
    if !allow_remote && !is_local_url(url) {
        return Err(format!("`{url}` is blocked: this panel is dev-server-only ([browser] allow_remote = false)."));
    }
    Ok(())
}

/// Schemes the panel may load. Anything else (`file:`, `javascript:`, `data:`)
/// is refused before it reaches the engine — a browsing surface inside the app
/// must not be a way to read local files or run script against our origin.
pub fn is_allowed_scheme(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower == "about:blank"
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("localhost")
        || lower.starts_with("127.0.0.1")
}

/// Turn what a human typed into a URL we are willing to load.
///
/// `:3000` -> `http://localhost:3000`, `localhost:3000` -> `http://localhost:3000`,
/// bare hosts get `https://`, and refused schemes come back as an error the
/// page shows as a card instead of a silent no-op.
pub(super) fn normalize_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Enter a URL to open.".into());
    }
    let lower = raw.to_ascii_lowercase();
    // Refuse explicit non-web schemes with a message that says why.
    if let Some(scheme) = lower.split(':').next() {
        if matches!(scheme, "file" | "javascript" | "data" | "vbscript" | "chrome" | "edge") {
            return Err(format!("`{scheme}:` URLs are blocked in the preview panel."));
        }
    }
    if lower == "about:blank" || lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(raw.to_string());
    }
    // Host:port shorthands people actually type.
    if let Some(rest) = raw.strip_prefix(':') {
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) && rest.chars().all(|c| c.is_ascii_digit()) {
            return Ok(format!("http://localhost:{rest}"));
        }
    }
    let first = raw.split(['/', '?', '#']).next().unwrap_or(raw);
    if lower.starts_with("localhost") || lower.starts_with("127.0.0.1") {
        return Ok(format!("http://{raw}"));
    }
    // Bare IPv4 (optionally with a port) is a local server, so plain http.
    let host = first.split(':').next().unwrap_or(first);
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() == 4 && octets.iter().all(|o| !o.is_empty() && o.chars().all(|c| c.is_ascii_digit())) {
        return Ok(format!("http://{raw}"));
    }
    if !host.contains('.') {
        return Err(format!("`{raw}` is not a URL — try `example.com` or `localhost:3000`."));
    }
    Ok(format!("https://{raw}"))
}

/// True for URLs that stay on this machine. Used to enforce the
/// dev-server-only mode (`[browser] allow_remote = false`).
///
/// The URL is parsed with the same rules a browser uses (the `url` crate,
/// re-exported by `reqwest`), so the host we check is the host the preview
/// really connects to. Cutting the text by hand got forms like
/// `http://name:password@host/` wrong.
pub fn is_local_url(url: &str) -> bool {
    let url = url.trim();
    if url.eq_ignore_ascii_case("about:blank") {
        return true;
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    match parsed.host_str() {
        // The parser lower-cases host names, so `LOCALHOST` arrives here too.
        Some("localhost") => true,
        // IP addresses: any loopback address (127.0.0.0/8 or ::1), or the
        // "all interfaces" address 0.0.0.0 that dev servers often print.
        // `host_str()` keeps the brackets around IPv6, so strip them first.
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback() || ip.is_unspecified())
            .unwrap_or(false),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A-F1: everything a person might paste into the URL bar has to turn into
    /// something the engine will actually load.
    #[test]
    fn url_input_normalizes_to_a_loadable_url() {
        assert_eq!(normalize_url("  https://example.com/a?b=1  ").unwrap(), "https://example.com/a?b=1");
        assert_eq!(normalize_url("http://localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(normalize_url(":5173").unwrap(), "http://localhost:5173");
        assert_eq!(normalize_url("localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(normalize_url("localhost").unwrap(), "http://localhost");
        assert_eq!(normalize_url("127.0.0.1:8080/app").unwrap(), "http://127.0.0.1:8080/app");
        assert_eq!(normalize_url("192.168.1.5:8000").unwrap(), "http://192.168.1.5:8000");
        assert_eq!(normalize_url("news.ycombinator.com").unwrap(), "https://news.ycombinator.com");
        assert_eq!(normalize_url("about:blank").unwrap(), "about:blank");
        // Nonsense is reported, not silently dropped.
        assert!(normalize_url("   ").is_err());
        assert!(normalize_url("just-a-word").is_err());
    }

    /// A-B3: `file:` would read local disk, `javascript:` would run script
    /// against whatever origin the panel happens to be on. Both refused.
    #[test]
    fn refused_schemes_are_reported_with_reasons() {
        for bad in ["file:///C:/Windows/win.ini", "javascript:alert(1)", "data:text/html,hi"] {
            let err = normalize_url(bad).expect_err(bad);
            assert!(err.contains("blocked"), "{bad} -> {err}");
        }
        assert!(!is_allowed_scheme("file:///etc/passwd"));
        assert!(!is_allowed_scheme("JavaScript:alert(1)"));
        assert!(is_allowed_scheme("https://example.com"));
        assert!(is_allowed_scheme("about:blank"));
    }

    /// S-11: one rule for typed URLs, links, redirects, the dev-server
    /// button and agent navigation.
    #[test]
    fn check_navigation_applies_scheme_and_allow_remote() {
        assert!(check_navigation("http://localhost:3000/", false).is_ok());
        assert!(check_navigation("about:blank", false).is_ok());
        assert!(check_navigation("https://example.com/", true).is_ok());
        let remote = check_navigation("https://example.com/", false).unwrap_err();
        assert!(remote.contains("allow_remote"), "{remote}");
        let scheme = check_navigation("file:///etc/hosts", true).unwrap_err();
        assert!(scheme.contains("http(s)"), "{scheme}");
    }

    /// A-B4: `allow_remote = false` must keep the panel on this machine.
    #[test]
    fn local_url_detection_covers_dev_servers() {
        for u in [
            "http://localhost:3000",
            "http://127.0.0.1:5173/app",
            "https://localhost",
            "http://0.0.0.0:8080",
            "http://[::1]:8080",
            "about:blank",
        ] {
            assert!(is_local_url(u), "{u} should count as local");
        }
        for u in
            ["https://example.com", "http://192.168.1.5:3000", "https://localhost.evil.example", "file:///etc/passwd"]
        {
            assert!(!is_local_url(u), "{u} should not count as local");
        }
    }

    /// S-10: the host we check must be the host the browser really uses.
    #[test]
    fn local_url_detection_reads_the_real_host() {
        for u in [
            "http://localhost:x@example.com/",
            "http://localhost@example.com/",
            "http://127.0.0.1:80@example.com/",
            "http://[::1]@example.com/",
            "http://localhost.example.com/",
            "http://example.com/?next=http://localhost:3000",
            "http://example.com/#localhost",
        ] {
            assert!(!is_local_url(u), "{u} is a remote URL");
        }
        // A user name in front of a real local host is still local.
        assert!(is_local_url("http://user:pw@localhost:3000/"));
    }

    /// S-10: IPv6, upper case, the whole loopback range, and junk input.
    #[test]
    fn local_url_detection_handles_ipv6_case_and_junk() {
        for u in [
            "http://[::1]/",
            "http://[0:0:0:0:0:0:0:1]:8080/app",
            "HTTP://LOCALHOST:3000",
            "http://127.0.0.2:5173",
            "  http://localhost:3000  ",
            "ABOUT:BLANK",
        ] {
            assert!(is_local_url(u), "{u} is local");
        }
        for u in ["http://[::2]/", "http://10.0.0.1/", "ftp://localhost/", "localhost:3000", ""] {
            assert!(!is_local_url(u), "{u} is not a local http(s) URL");
        }
    }
}
