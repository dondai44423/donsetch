//! Immutable routing policy shared by a fetch, browser recovery and replay.

use super::proxy::{Proxy, env_proxy_var, no_proxy_match_value, no_proxy_value};
use crate::error::FetchError;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RequestRoute(RoutePolicy);

#[derive(Clone, PartialEq, Eq, Hash)]
enum RoutePolicy {
    Direct,
    Pinned(Proxy),
    Configured {
        http: Option<String>,
        https: Option<String>,
        bypass: String,
    },
}

impl std::fmt::Debug for RequestRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RequestRoute").field(&self.id()).finish()
    }
}

impl RequestRoute {
    pub fn direct() -> Self {
        Self(RoutePolicy::Direct)
    }
    pub fn pinned(proxy: Proxy) -> Self {
        Self(RoutePolicy::Pinned(proxy))
    }

    pub fn configured() -> Self {
        let http = configured_proxy_value("http");
        let https = configured_proxy_value("https");
        if http.is_none() && https.is_none() {
            return Self::direct();
        }
        Self(RoutePolicy::Configured {
            http,
            https,
            bypass: no_proxy_value(),
        })
    }

    /// Opaque identity; endpoint credentials never enter browser keys or receipts.
    pub fn id(&self) -> String {
        use sha2::{Digest, Sha256};
        match &self.0 {
            RoutePolicy::Direct => "direct".into(),
            RoutePolicy::Pinned(proxy) => format!("proxy:{}", proxy.connection_key()),
            RoutePolicy::Configured {
                http,
                https,
                bypass,
            } => {
                let mut hash = Sha256::new();
                for value in [http.as_deref(), https.as_deref(), Some(bypass.as_str())] {
                    hash.update([u8::from(value.is_some())]);
                    let value = value.unwrap_or_default();
                    hash.update((value.len() as u64).to_be_bytes());
                    hash.update(value.as_bytes());
                }
                let digest: String = hash
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                format!("configured:{digest}")
            }
        }
    }

    pub fn proxy_for(&self, url: &str) -> Result<Option<Proxy>, FetchError> {
        let parsed = url::Url::parse(url)
            .map_err(|_| FetchError::InvalidUrl("route requires a valid HTTP(S) URL".into()))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| FetchError::InvalidUrl("route requires a host".into()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(FetchError::InvalidUrl(
                "route requires an HTTP(S) URL".into(),
            ));
        }
        match &self.0 {
            RoutePolicy::Direct => Ok(None),
            RoutePolicy::Pinned(proxy) => Ok(Some(proxy.clone())),
            RoutePolicy::Configured {
                http,
                https,
                bypass,
            } => {
                if no_proxy_match_value(host, bypass) {
                    return Ok(None);
                }
                parse_selected(if parsed.scheme() == "https" {
                    https
                } else {
                    http
                })
            }
        }
    }

    /// Native HTTP/HTTPS proxy map plus HTTP's exact host-only bypass policy.
    pub(crate) fn browser_proxies(
        &self,
    ) -> Result<(Option<Proxy>, Option<Proxy>, String), FetchError> {
        match &self.0 {
            RoutePolicy::Direct => Ok((None, None, String::new())),
            RoutePolicy::Pinned(proxy) => {
                Ok((Some(proxy.clone()), Some(proxy.clone()), String::new()))
            }
            RoutePolicy::Configured {
                http,
                https,
                bypass,
            } => {
                if bypass.split(',').any(|entry| entry.trim() == "*") {
                    return Ok((None, None, String::new()));
                }
                Ok((
                    parse_selected(http)?,
                    parse_selected(https)?,
                    bypass.clone(),
                ))
            }
        }
    }
}

fn parse_selected(value: &Option<String>) -> Result<Option<Proxy>, FetchError> {
    value
        .as_deref()
        .map(Proxy::parse)
        .transpose()
        .map_err(|error| match error {
            FetchError::Http(message) => FetchError::ProxyConfig(message),
            other => FetchError::ProxyConfig(other.to_string()),
        })
}

fn configured_proxy_value(scheme: &str) -> Option<String> {
    let cfg = crate::config::cfg();
    let (slot, env_name) = if scheme == "https" {
        (&cfg.proxy.https, "HTTPS_PROXY")
    } else {
        (&cfg.proxy.http, "HTTP_PROXY")
    };
    if let Some(value) = [slot, &cfg.proxy.all]
        .into_iter()
        .find(|value| !value.trim().is_empty())
    {
        return Some(value.trim().to_owned());
    }
    if !cfg.proxy.from_environment {
        return None;
    }
    env_proxy_var(env_name)
        .or_else(|| env_proxy_var(&env_name.to_lowercase()))
        .or_else(|| env_proxy_var("ALL_PROXY"))
        .or_else(|| env_proxy_var("all_proxy"))
        .map(|value| value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stealth_v3_route_snapshot_keeps_protocol_bypass_and_credentials() {
        let route = RequestRoute(RoutePolicy::Configured {
            http: Some("http://alice:owned-secret@proxy-a:8080".into()),
            https: Some("socks5://proxy-b:1080".into()),
            bypass: ".owned.example:8443,192.168.0.0/16,[::1]:8888".into(),
        });
        assert_eq!(
            route
                .proxy_for("http://example.org/")
                .unwrap()
                .unwrap()
                .host,
            "proxy-a"
        );
        assert_eq!(
            route
                .proxy_for("https://example.org/")
                .unwrap()
                .unwrap()
                .host,
            "proxy-b"
        );
        for url in [
            "http://owned.example:80/",
            "https://child.owned.example/",
            "http://192.168.2.3/",
            "https://[::1]/",
        ] {
            assert!(route.proxy_for(url).unwrap().is_none(), "{url}");
        }
        assert!(
            route
                .proxy_for("http://other-owned.example/")
                .unwrap()
                .is_some()
        );
        let before = route.proxy_for("http://example.org/").unwrap();
        unsafe {
            std::env::set_var("HTTP_PROXY", "http://changed:9000");
            std::env::set_var("NO_PROXY", "*");
        }
        assert_eq!(route.proxy_for("http://example.org/").unwrap(), before);
        let (http, https, bypass) = route.browser_proxies().unwrap();
        assert_eq!(http, before);
        assert_eq!(https.unwrap().host, "proxy-b");
        assert!(bypass.contains("owned.example"));
        assert!(!format!("{route:?}").contains("alice") && !route.id().contains("owned-secret"));
        assert_ne!(route.id(), RequestRoute::pinned(before.unwrap()).id());
    }

    #[test]
    fn stealth_v3_route_direct_and_pinned_are_explicit() {
        let proxy = Proxy::parse("http://alice:owned-secret@proxy-a:8080").unwrap();
        let pinned = RequestRoute::pinned(proxy.clone());
        for url in ["http://127.0.0.1/", "https://example.org/"] {
            assert_eq!(pinned.proxy_for(url).unwrap(), Some(proxy.clone()));
            assert!(RequestRoute::direct().proxy_for(url).unwrap().is_none());
        }
        let other =
            RequestRoute::pinned(Proxy::parse("http://alice:changed@proxy-a:8080").unwrap());
        assert_ne!(pinned.id(), other.id());
        assert!(pinned.proxy_for("file:///tmp/owned").is_err());
    }
}
