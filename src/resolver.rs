use std::collections::HashMap;

use crate::config::{Config, DnsConfig};
use lazy_static::lazy_static;
use log::{error, info};
use regex::{Regex, RegexBuilder};
use std::sync::Arc;
use tokio::sync::RwLock;

// DNS overrides. Resolution priority (first hit wins):
//   1. exact host:port
//   2. exact host (any port)
//   3. suffix rules with a port (longest suffix first)
//   4. suffix rules without a port (longest suffix first)
//   5. regex rules, in definition order (hostname-only match, optional port filter)

#[derive(Debug, Clone)]
struct SuffixRule {
    /// suffix with any leading dot removed, lowercased
    base: String,
    port: Option<u16>,
    to: String,
}

#[derive(Debug)]
struct RegexRule {
    pattern: Regex,
    port: Option<u16>,
    to: String,
}

#[derive(Default)]
struct Tables {
    exact_with_port: HashMap<(String, u16), String>,
    exact_any_port: HashMap<String, String>,
    /// port-qualified rules first, then port-less; longest base first within each group
    suffix: Vec<SuffixRule>,
    regex: Vec<RegexRule>,
}

lazy_static! {
    static ref TABLES: Arc<RwLock<Tables>> = Arc::new(RwLock::new(Tables::default()));
}

/// Splits `value` into (host, Some(port)) when it ends with `:<u16>`,
/// (value, None) otherwise. IPv6 literals in brackets keep working because a
/// trailing `:port` is only recognized after the last colon.
fn split_host_port(value: &str) -> (String, Option<u16>) {
    if let Some((host, port)) = value.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            return (host.to_lowercase(), Some(port));
        }
    }
    (value.to_lowercase(), None)
}

pub async fn init(config: &Config) {
    info!("initializing DNS override");
    let count = init_inner(&config.dns).await;
    info!("initialized DNS override. {count} rules loaded");
}

async fn init_inner(dns: &DnsConfig) -> usize {
    let mut tables = Tables::default();

    for rule in &dns.exact {
        let (host, port) = split_host_port(rule.from.trim());
        match port {
            Some(port) => {
                info!("DNS rule: exact {host}:{port} -> {}", rule.to);
                tables.exact_with_port.insert((host, port), rule.to.clone());
            }
            None => {
                info!("DNS rule: exact {host} (any port) -> {}", rule.to);
                tables.exact_any_port.insert(host, rule.to.clone());
            }
        }
    }

    for rule in &dns.suffix {
        let (suffix, port) = split_host_port(rule.from.trim());
        let base = suffix.trim_start_matches('.').to_string();
        if base.is_empty() {
            error!("DNS rule: suffix `{}` is empty; skipping", rule.from);
            continue;
        }
        match port {
            Some(port) => info!("DNS rule: suffix .{base}:{port} -> {}", rule.to),
            None => info!("DNS rule: suffix .{base} (any port) -> {}", rule.to),
        }
        tables.suffix.push(SuffixRule {
            base,
            port,
            to: rule.to.clone(),
        });
    }
    // port-qualified before port-less; longest suffix first within each group.
    // sort_by_key is stable, so equal keys keep their definition order.
    tables
        .suffix
        .sort_by_key(|rule| (rule.port.is_none(), std::cmp::Reverse(rule.base.len())));

    for rule in &dns.regex {
        let pattern = RegexBuilder::new(&rule.hostname)
            .case_insensitive(true)
            .build();
        match pattern {
            Ok(pattern) => {
                info!(
                    "DNS rule: regex {} (port {:?}) -> {}",
                    rule.hostname, rule.port, rule.to
                );
                tables.regex.push(RegexRule {
                    pattern,
                    port: rule.port,
                    to: rule.to.clone(),
                });
            }
            Err(cause) => {
                error!(
                    "DNS rule: invalid regex `{}` ({cause}); skipping",
                    rule.hostname
                );
            }
        }
    }

    let count = tables.exact_with_port.len()
        + tables.exact_any_port.len()
        + tables.suffix.len()
        + tables.regex.len();
    *TABLES.write().await = tables;
    count
}

/// True when `host` equals `base` or ends with `.base` (label boundary).
fn suffix_matches(host: &str, base: &str) -> bool {
    host == base
        || (host.len() > base.len()
            && host.ends_with(base)
            && host.as_bytes()[host.len() - base.len() - 1] == b'.')
}

/// Resolves an override for `host` (an SNI hostname, no port) and `port`.
/// Returns the replacement target (`host` or `host:port`) or None when no
/// rule matches and normal DNS should apply.
pub async fn resolve(host: &str, port: u16) -> Option<String> {
    let host = host.to_lowercase();
    let tables = TABLES.read().await;

    if let Some(to) = tables.exact_with_port.get(&(host.clone(), port)) {
        info!("DNS exact match {host}:{port} -> {to}");
        return Some(to.clone());
    }
    if let Some(to) = tables.exact_any_port.get(&host) {
        info!("DNS exact match {host} (any port) -> {to}");
        return Some(to.clone());
    }

    for rule in &tables.suffix {
        if rule.port.is_some_and(|rule_port| rule_port != port) {
            continue;
        }
        if suffix_matches(&host, &rule.base) {
            info!(
                "DNS suffix match {host}:{port} -> {} by suffix `.{}`",
                rule.to, rule.base
            );
            return Some(rule.to.clone());
        }
    }

    for rule in &tables.regex {
        if rule.port.is_some_and(|rule_port| rule_port != port) {
            continue;
        }
        if rule.pattern.is_match(&host) {
            info!(
                "DNS regex match {host}:{port} -> {} by regex `{}`",
                rule.to, rule.pattern
            );
            return Some(rule.to.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExactDnsRule, RegexDnsRule, SuffixDnsRule};

    fn exact(from: &str, to: &str) -> ExactDnsRule {
        ExactDnsRule {
            from: from.into(),
            to: to.into(),
        }
    }

    fn suffix(from: &str, to: &str) -> SuffixDnsRule {
        SuffixDnsRule {
            from: from.into(),
            to: to.into(),
        }
    }

    fn regex(hostname: &str, port: Option<u16>, to: &str) -> RegexDnsRule {
        RegexDnsRule {
            hostname: hostname.into(),
            port,
            to: to.into(),
        }
    }

    #[tokio::test]
    async fn resolution_priority_is_exact_then_suffix_then_regex() {
        init_inner(&DnsConfig {
            exact: vec![
                exact("api.internal.example.com:443", "exact-port.example:443"),
                exact("api.internal.example.com", "exact-any.example:443"),
            ],
            suffix: vec![
                suffix(".example.com", "suffix-short.example:443"),
                suffix(".internal.example.com:443", "suffix-port.example:443"),
                suffix(".internal.example.com", "suffix-long.example:443"),
            ],
            regex: vec![
                regex("^api\\.", None, "regex-first.example:443"),
                regex("example", None, "regex-second.example:443"),
            ],
        })
        .await;

        // exact host:port beats exact host beats everything else
        assert_eq!(
            resolve("api.internal.example.com", 443).await.as_deref(),
            Some("exact-port.example:443")
        );
        assert_eq!(
            resolve("api.internal.example.com", 8443).await.as_deref(),
            Some("exact-any.example:443")
        );
        // suffix with port beats port-less suffix of the same length
        assert_eq!(
            resolve("web.internal.example.com", 443).await.as_deref(),
            Some("suffix-port.example:443")
        );
        // port-less long suffix beats short one when the port rule filters out
        assert_eq!(
            resolve("web.internal.example.com", 8443).await.as_deref(),
            Some("suffix-long.example:443")
        );
        // regex only fires when nothing above matched, in definition order
        assert_eq!(
            resolve("api.other.test", 443).await.as_deref(),
            Some("regex-first.example:443")
        );
        assert_eq!(resolve("nothing.test", 443).await.as_deref(), None);
    }

    #[tokio::test]
    async fn suffix_respects_label_boundaries_and_matches_bare_domain() {
        init_inner(&DnsConfig {
            exact: vec![],
            suffix: vec![suffix(".abc.com", "hit.example")],
            regex: vec![],
        })
        .await;

        assert_eq!(
            resolve("x.abc.com", 443).await.as_deref(),
            Some("hit.example")
        );
        assert_eq!(
            resolve("abc.com", 443).await.as_deref(),
            Some("hit.example")
        );
        assert_eq!(resolve("notabc.com", 443).await, None);
    }

    #[tokio::test]
    async fn regex_matches_hostname_without_port_and_honors_port_filter() {
        init_inner(&DnsConfig {
            exact: vec![],
            suffix: vec![],
            regex: vec![
                regex("^api\\d+\\.abc\\.com$", Some(443), "https.example:443"),
                regex("^api\\d+\\.abc\\.com$", None, "any.example"),
            ],
        })
        .await;

        assert_eq!(
            resolve("api1.abc.com", 443).await.as_deref(),
            Some("https.example:443")
        );
        assert_eq!(
            resolve("api1.abc.com", 8443).await.as_deref(),
            Some("any.example")
        );
        // the pattern sees only the hostname; a port in the pattern never matches
        init_inner(&DnsConfig {
            exact: vec![],
            suffix: vec![],
            regex: vec![regex("abc\\.com:443", None, "never.example")],
        })
        .await;
        assert_eq!(resolve("x.abc.com", 443).await, None);
    }

    #[tokio::test]
    async fn invalid_regex_is_skipped_not_fatal() {
        let count = init_inner(&DnsConfig {
            exact: vec![exact("ok.test", "target.test")],
            suffix: vec![],
            regex: vec![regex("(unclosed", None, "broken.example")],
        })
        .await;
        assert_eq!(count, 1);
        assert_eq!(
            resolve("ok.test", 443).await.as_deref(),
            Some("target.test")
        );
    }
}
