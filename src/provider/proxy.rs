//! Go ProxyFromEnvironment routing: loopback is always direct, NO_PROXY applies
//! before HTTP_PROXY/HTTPS_PROXY, and ALL_PROXY is deliberately ignored. Proxy
//! URLs may use `http`, `https`, `socks4`, `socks4a`, `socks5`, or `socks5h`.

use std::net::IpAddr;

use reqwest::{ClientBuilder, Proxy, Url};

struct Selector {
    http: Option<Url>,
    https: Option<Url>,
    bypass: Vec<Matcher>,
}

enum Matcher {
    All,
    Network(IpAddr, u8),
    Host { host: String, port: Option<String> },
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    }
}

fn parse_ip(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

fn proxy_url(value: &str) -> Option<Url> {
    if value.is_empty() {
        return None;
    }
    let parsed = if value.contains("://") {
        Url::parse(value).ok()?
    } else {
        Url::parse(&format!("http://{value}")).ok()?
    };
    (matches!(
        parsed.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) && parsed.host_str().is_some())
    .then_some(parsed)
}

impl Selector {
    fn new(http: &str, https: &str, no_proxy: &str) -> Self {
        let bypass = no_proxy
            .split(',')
            .filter_map(|entry| {
                let entry = entry.trim().to_ascii_lowercase();
                if entry.is_empty() {
                    return None;
                }
                if entry == "*" {
                    return Some(Matcher::All);
                }
                if let Some((network, prefix)) = entry.split_once('/') {
                    let network: IpAddr = network.parse().ok()?;
                    let prefix: u8 = prefix.parse().ok()?;
                    let max = if network.is_ipv4() { 32 } else { 128 };
                    // Go's net.IPNet treats mapped IPv4 networks as IPv4 when
                    // the prefix includes all 96 mapping bits.
                    if let IpAddr::V6(v6) = network
                        && let Some(v4) = v6.to_ipv4_mapped()
                        && (96..=128).contains(&prefix)
                    {
                        return Some(Matcher::Network(IpAddr::V4(v4), prefix - 96));
                    }
                    return (prefix <= max).then_some(Matcher::Network(network, prefix));
                }
                let (host, port) = if entry.starts_with('[') {
                    match entry.split_once("]:") {
                        Some((host, port)) => {
                            (host.trim_start_matches('['), Some(port.to_string()))
                        }
                        None => (entry.trim_start_matches('[').trim_end_matches(']'), None),
                    }
                } else if entry.matches(':').count() == 1 {
                    let (host, port) = entry.split_once(':').unwrap();
                    (host, Some(port.to_string()))
                } else {
                    (entry.as_str(), None)
                };
                if host.is_empty() {
                    return None;
                }
                Some(Matcher::Host {
                    host: host.to_string(),
                    port: port.filter(|p| !p.is_empty()),
                })
            })
            .collect();
        Self {
            http: proxy_url(http),
            https: proxy_url(https),
            bypass,
        }
    }

    fn select(&self, url: &Url) -> Option<Url> {
        let host = url
            .host_str()?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let ip = parse_ip(&host).map(unmap);
        if host == "localhost" || ip.is_some_and(|ip| ip.is_loopback()) {
            return None;
        }
        if self
            .bypass
            .iter()
            .any(|rule| rule.matches(&host, url.port_or_known_default(), ip))
        {
            return None;
        }
        match url.scheme() {
            "http" => self.http.clone(),
            "https" => self.https.clone(),
            _ => None,
        }
    }
}

impl Matcher {
    fn matches(&self, host: &str, port: Option<u16>, ip: Option<IpAddr>) -> bool {
        match self {
            Self::All => true,
            Self::Network(network, prefix) => {
                let Some(ip) = ip else {
                    return false;
                };
                match (*network, ip) {
                    (IpAddr::V4(net), IpAddr::V4(ip)) => {
                        let mask = u32::MAX.checked_shl(32 - u32::from(*prefix)).unwrap_or(0);
                        u32::from(net) & mask == u32::from(ip) & mask
                    }
                    (IpAddr::V6(net), IpAddr::V6(ip)) => {
                        let mask = u128::MAX.checked_shl(128 - u32::from(*prefix)).unwrap_or(0);
                        u128::from(net) & mask == u128::from(ip) & mask
                    }
                    _ => false,
                }
            }
            Self::Host {
                host: entry,
                port: entry_port,
            } => {
                if entry_port
                    .as_ref()
                    .is_some_and(|wanted| port.map(|p| p.to_string()).as_ref() != Some(wanted))
                {
                    return false;
                }
                if let Some(wanted) = parse_ip(entry) {
                    return ip == Some(unmap(wanted));
                }
                if ip.is_some() {
                    return false;
                }
                let entry = if entry.starts_with("*.") {
                    &entry[1..]
                } else {
                    entry
                };
                if entry.starts_with('.') {
                    host.ends_with(entry)
                } else {
                    host == entry
                        || host
                            .strip_suffix(entry)
                            .is_some_and(|prefix| prefix.ends_with('.'))
                }
            }
        }
    }
}

/// Snapshot environment once per client, as Go does for its proxy transport.
/// Always disable reqwest's defaults so ALL_PROXY cannot re-enable routing.
pub(crate) fn configure(builder: ClientBuilder) -> ClientBuilder {
    let env = |upper: &str, lower: &str| {
        std::env::var(upper)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var(lower).ok())
            .unwrap_or_default()
    };
    let selector = Selector::new(
        &env("HTTP_PROXY", "http_proxy"),
        &env("HTTPS_PROXY", "https_proxy"),
        &env("NO_PROXY", "no_proxy"),
    );
    let builder = builder.no_proxy();
    if selector.http.is_some() || selector.https.is_some() {
        builder.proxy(Proxy::custom(move |url| selector.select(url)))
    } else {
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(no_proxy: &str, target: &str) -> Option<String> {
        Selector::new(
            "http://http-proxy:8080",
            "http://https-proxy:8081",
            no_proxy,
        )
        .select(&Url::parse(target).unwrap())
        .map(|url| url.to_string())
    }

    #[test]
    fn loopback_is_always_direct() {
        for target in [
            "http://localhost/",
            "https://LOCALHOST:8443/",
            "http://127.0.0.1/",
            "http://127.42.3.4/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.2]/",
        ] {
            assert_eq!(routed("", target), None, "{target}");
        }
        assert!(parse_ip("::1").unwrap().is_loopback());
        assert!(parse_ip("[::1]").unwrap().is_loopback());
    }

    #[test]
    fn no_proxy_matches_go_host_ip_cidr_and_port_forms() {
        for (entry, target, bypass) in [
            ("*", "http://anything.example/", true),
            ("example.com", "http://example.com/", true),
            ("EXAMPLE.com", "http://api.example.com/", true),
            ("example.com", "http://notexample.com/", false),
            (".example.com", "http://example.com/", false),
            (".example.com", "http://api.example.com/", true),
            ("*.example.com", "http://api.example.com/", true),
            ("*example.com", "http://example.com/", false),
            ("example.com:80", "http://example.com/", true),
            ("example.com:80", "https://example.com/", false),
            ("example.com:8080", "http://api.example.com:8080/", true),
            ("192.0.2.1", "http://192.0.2.1:1234/", true),
            ("192.0.2.1:80", "http://192.0.2.1:1234/", false),
            ("192.0.2.0/24", "http://192.0.2.99/", true),
            ("192.0.2.0/24", "http://192.0.3.1/", false),
            ("0.0.0.0/0", "http://192.0.2.1/", true),
            ("192.0.2.0/33", "http://192.0.2.1/", false),
            ("2001:db8::1", "http://[2001:db8::1]/", true),
            ("[2001:db8::1]:443", "https://[2001:db8::1]/", true),
            ("[2001:db8::1]:443", "http://[2001:db8::1]/", false),
            ("2001:db8::/32", "http://[2001:db8:1::1]/", true),
            ("2001:db8::/32", "http://[2001:db9::1]/", false),
            ("192.0.2.1", "http://[::ffff:192.0.2.1]/", true),
            ("::ffff:192.0.2.0/120", "http://192.0.2.1/", true),
            ("::ffff:192.0.2.0/120", "http://[::ffff:192.0.3.1]/", false),
            (
                " ,invalid/garbage, .example.com ",
                "http://api.example.com/",
                true,
            ),
        ] {
            assert_eq!(
                routed(entry, target).is_none(),
                bypass,
                "{entry} -> {target}"
            );
        }
    }

    #[test]
    fn socks_proxy_urls_are_selected_and_loopback_still_bypasses() {
        // Go's http.DefaultTransport accepts SOCKS proxies; reqwest needs the
        // `socks` feature to parse these schemes. Selection is pure here; the
        // handshake is reqwest's own SOCKS implementation.
        assert_eq!(
            routed("", "https://example.com/").as_deref(),
            Some("http://https-proxy:8081/")
        );
        for scheme in ["socks4", "socks4a", "socks5", "socks5h"] {
            let proxy = format!("{scheme}://127.0.0.1:1080");
            let selector = Selector::new("", &proxy, "");
            assert_eq!(
                selector
                    .select(&Url::parse("https://example.com/").unwrap())
                    .map(|url| url.to_string()),
                Some(format!("{scheme}://127.0.0.1:1080")),
                "{scheme}"
            );
        }
        let selector = Selector::new("", "socks5://127.0.0.1:1080", "");
        for target in [
            "https://localhost/",
            "https://127.0.0.1/",
            "https://127.42.3.4:8443/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.2]/",
        ] {
            assert_eq!(
                selector.select(&Url::parse(target).unwrap()),
                None,
                "{target}"
            );
        }

        // With the `socks` feature compiled in, reqwest accepts the same URL.
        assert!(reqwest::Proxy::all("socks5://127.0.0.1:1080").is_ok());
    }

    #[test]
    fn proxy_selection_uses_target_scheme_and_handles_invalid_urls() {
        assert_eq!(
            routed("", "http://example.com/").as_deref(),
            Some("http://http-proxy:8080/")
        );
        assert_eq!(
            routed("", "https://example.com/").as_deref(),
            Some("http://https-proxy:8081/")
        );
        assert_eq!(
            proxy_url("proxy.example:3128").unwrap().as_str(),
            "http://proxy.example:3128/"
        );
        assert!(proxy_url("http://[").is_none());
        assert!(
            Selector::new("http://[", "", "")
                .select(&Url::parse("http://example.com/").unwrap())
                .is_none()
        );
    }
}
