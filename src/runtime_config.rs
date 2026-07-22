use std::collections::HashMap;

use anyhow::{bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::acme_types::{AcmeSettings, ControlPlaneConfig};
use crate::config::{
    AccountingConfig, Config as LegacyConfig, DnsConfig, Listener, ListenerMode, Policy, Rules,
};

pub const DEFAULT_LISTENER_NAME: &str = "__default_https__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub default_listener: DefaultListenerConfig,
    #[serde(default)]
    pub additional_listeners: HashMap<String, AdditionalListenerConfig>,
    #[serde(default)]
    pub dns_overrides: DnsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<AccountingConfig>,
    #[serde(default)]
    pub control_plane: ControlPlaneConfig,
    #[serde(default)]
    pub acme: AcmeSettings,
    #[serde(default)]
    pub certificate_fallback: CertificateFallbackPolicy,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            default_listener: DefaultListenerConfig::default(),
            additional_listeners: HashMap::new(),
            dns_overrides: DnsConfig::default(),
            accounting: None,
            control_plane: ControlPlaneConfig::default(),
            acme: AcmeSettings::default(),
            certificate_fallback: CertificateFallbackPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultListenerConfig {
    /// Mandatory public listener. The port must be 443; the UI may change the
    /// address/interface portion but cannot delete or disable this listener.
    #[serde(default = "default_https_bind")]
    pub bind: String,
    #[serde(default)]
    pub ordinary_traffic: OrdinaryTlsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "lowercase")]
pub enum AdditionalListenerConfig {
    /// Hostname-aware TLS listener. Every SNI can select passthrough,
    /// terminate-to-plaintext, terminate-to-TLS, or reject independently.
    Tls(HostRoutedTlsListenerConfig),
    /// Plain HTTP listener. Host routing selects its destination/transport;
    /// TLS passthrough is impossible because the downstream is not TLS.
    Http(HostRoutedHttpListenerConfig),
    /// Raw byte forwarding has no hostname and therefore no host route table.
    Forward(RawForwardListenerConfig),
}

impl AdditionalListenerConfig {
    pub fn bind(&self) -> &str {
        match self {
            Self::Tls(config) => &config.bind,
            Self::Http(config) => &config.bind,
            Self::Forward(config) => &config.bind,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRoutedTlsListenerConfig {
    pub bind: String,
    #[serde(default)]
    pub routing: OrdinaryTlsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRoutedHttpListenerConfig {
    pub bind: String,
    #[serde(default)]
    pub routes: Vec<HttpHostRoute>,
    #[serde(default = "default_allow_policy")]
    pub unknown_host_policy: Policy,
    #[serde(default = "empty_rules")]
    pub unknown_host_rules: Rules,
    #[serde(default)]
    pub default_route: HttpRouteAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_idle_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_limit: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHostRoute {
    #[serde(default)]
    pub name: String,
    pub matcher: HostMatcher,
    pub action: HttpRouteAction,
}

impl HostRoutedHttpListenerConfig {
    pub fn select_route(&self, host: &str) -> Option<&HttpRouteAction> {
        if let Some(route) = self.routes.iter().find(|route| route.matcher.matches(host)) {
            return Some(&route.action);
        }
        let matched = self
            .unknown_host_rules
            .static_hosts
            .iter()
            .any(|entry| host.eq_ignore_ascii_case(entry))
            || self
                .unknown_host_rules
                .patterns
                .iter()
                .any(|pattern| pattern.is_match(host));
        let allowed = match self.unknown_host_policy {
            Policy::ALLOW => matched,
            Policy::DENY => !matched,
        };
        allowed.then_some(&self.default_route)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRouteAction {
    #[serde(default = "default_http_target_port")]
    pub target_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub upstream: UpstreamTransport,
}

impl Default for HttpRouteAction {
    fn default() -> Self {
        Self {
            target_port: default_http_target_port(),
            target: None,
            upstream: UpstreamTransport::Plaintext,
        }
    }
}

fn default_http_target_port() -> u16 {
    80
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawForwardListenerConfig {
    pub bind: String,
    pub targets: String,
    #[serde(default)]
    pub upstream_tls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_idle_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_limit: Option<f64>,
}

impl Default for DefaultListenerConfig {
    fn default() -> Self {
        Self {
            bind: default_https_bind(),
            ordinary_traffic: OrdinaryTlsConfig::default(),
        }
    }
}

fn default_https_bind() -> String {
    "0.0.0.0:443".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrdinaryTlsConfig {
    /// Ordered configured-host routes. These are explicit grants; the first
    /// matcher wins. Control-host and ACME decisions happen before this table.
    #[serde(default)]
    pub routes: Vec<TlsHostRoute>,
    /// Unknown hosts are admitted by this policy and then use `default_route`.
    /// ALLOW with empty rules is deliberately deny-all.
    #[serde(default = "default_allow_policy")]
    pub unknown_host_policy: Policy,
    #[serde(default = "empty_rules")]
    pub unknown_host_rules: Rules,
    #[serde(default)]
    pub default_route: TlsRouteAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_idle_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_limit: Option<f64>,
}

impl Default for OrdinaryTlsConfig {
    fn default() -> Self {
        Self {
            routes: Vec::new(),
            unknown_host_policy: Policy::ALLOW,
            unknown_host_rules: empty_rules(),
            default_route: TlsRouteAction::default(),
            max_idle_time_ms: None,
            speed_limit: None,
        }
    }
}

impl OrdinaryTlsConfig {
    /// Explicit configured-host routes are grants and win in definition order.
    /// Only an unknown host is evaluated against the fallback ACL.
    pub fn select_route(&self, host: &str) -> Option<&TlsRouteAction> {
        if let Some(route) = self.routes.iter().find(|route| route.matcher.matches(host)) {
            return Some(&route.action);
        }
        let matched = self
            .unknown_host_rules
            .static_hosts
            .iter()
            .any(|entry| host.eq_ignore_ascii_case(entry))
            || self
                .unknown_host_rules
                .patterns
                .iter()
                .any(|pattern| pattern.is_match(host));
        let allowed = match self.unknown_host_policy {
            Policy::ALLOW => matched,
            Policy::DENY => !matched,
        };
        allowed.then_some(&self.default_route)
    }
}

fn default_allow_policy() -> Policy {
    Policy::ALLOW
}

fn empty_rules() -> Rules {
    Rules {
        static_hosts: Vec::new(),
        patterns: Vec::<Regex>::new(),
    }
}

fn default_target_port() -> u16 {
    443
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsHostRoute {
    #[serde(default)]
    pub name: String,
    pub matcher: HostMatcher,
    pub action: TlsRouteAction,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostMatcher {
    #[serde(default)]
    pub exact: Vec<String>,
    #[serde(default)]
    pub suffix: Vec<String>,
    #[serde(default = "empty_patterns", with = "crate::config::ci_regex")]
    pub patterns: Vec<Regex>,
}

fn empty_patterns() -> Vec<Regex> {
    Vec::new()
}

impl HostMatcher {
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.');
        self.exact.iter().any(|entry| host.eq_ignore_ascii_case(entry.trim_end_matches('.')))
            || self.suffix.iter().any(|entry| {
                let suffix = entry.trim().trim_start_matches('.').trim_end_matches('.');
                host.eq_ignore_ascii_case(suffix)
                    || host.len() > suffix.len()
                        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
                        && host[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
            })
            || self.patterns.iter().any(|pattern| pattern.is_match(host))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TlsRouteAction {
    Passthrough {
        #[serde(default = "default_target_port")]
        target_port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    Terminate {
        #[serde(default = "default_target_port")]
        target_port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default)]
        upstream: UpstreamTransport,
    },
    Reject,
}

impl Default for TlsRouteAction {
    fn default() -> Self {
        Self::Passthrough {
            target_port: default_target_port(),
            target: None,
        }
    }
}

impl TlsRouteAction {
    pub fn target_port(&self) -> Option<u16> {
        match self {
            Self::Passthrough { target_port, .. }
            | Self::Terminate { target_port, .. } => Some(*target_port),
            Self::Reject => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamTransport {
    #[default]
    Plaintext,
    Tls,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CertificateFallbackPolicy {
    #[default]
    LocalCa,
    Reject,
}

impl RuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        crate::bindaddr::parse_bind_pattern(&self.default_listener.bind)
            .map_err(|cause| anyhow::anyhow!("default listener: {cause}"))?;
        if !bind_uses_port_443(&self.default_listener.bind) {
            bail!("default listener must bind port 443");
        }
        for route in self
            .default_listener
            .ordinary_traffic
            .routes
            .iter()
            .map(|route| &route.action)
            .chain(std::iter::once(
                &self.default_listener.ordinary_traffic.default_route,
            ))
        {
            if route.target_port() == Some(0) {
                bail!("default listener route target port must be non-zero");
            }
        }
        if let Some(accounting) = &self.accounting {
            accounting
                .validate()
                .map_err(|cause| anyhow::anyhow!(cause))?;
        }
        for (name, listener) in &self.additional_listeners {
            crate::bindaddr::parse_bind_pattern(listener.bind())
                .map_err(|cause| anyhow::anyhow!("listener `{name}`: {cause}"))?;
            if listener.bind() == self.default_listener.bind {
                bail!("listener `{name}` conflicts with the mandatory default listener");
            }
            match listener {
                AdditionalListenerConfig::Tls(listener) => {
                    for action in listener
                        .routing
                        .routes
                        .iter()
                        .map(|route| &route.action)
                        .chain(std::iter::once(&listener.routing.default_route))
                    {
                        if action.target_port() == Some(0) {
                            bail!("listener `{name}` TLS route target port must be non-zero");
                        }
                    }
                }
                AdditionalListenerConfig::Http(listener) => {
                    if listener
                        .routes
                        .iter()
                        .map(|route| route.action.target_port)
                        .chain(std::iter::once(listener.default_route.target_port))
                        .any(|port| port == 0)
                    {
                        bail!("listener `{name}` HTTP route target port must be non-zero");
                    }
                }
                AdditionalListenerConfig::Forward(listener) => {
                    if listener.targets.trim().is_empty() {
                        bail!("listener `{name}` forward targets are required");
                    }
                }
            }
        }
        if !self.control_plane.hostname.is_empty() {
            crate::store::normalize_domain(&self.control_plane.hostname)?;
        }
        for self_ip in &self.control_plane.self_ips {
            self_ip
                .parse::<std::net::IpAddr>()
                .map_err(|cause| anyhow::anyhow!("invalid self IP `{self_ip}`: {cause}"))?;
        }
        for resolver in &self.control_plane.public_resolvers {
            crate::acme::dns::parse_resolver(resolver)?;
        }
        if !(120..=1800).contains(&self.acme.challenge_ttl_seconds) {
            bail!("ACME challenge TTL must be between 120 and 1800 seconds");
        }
        if self.acme.scan_interval_hours == 0 {
            bail!("ACME scan interval must be non-zero");
        }
        Ok(())
    }

    /// Transitional conversion used only by the future explicit YAML migration
    /// command. Legacy listeners remain additional listeners; migration must
    /// never infer that an arbitrary old listener is the protected system one.
    pub fn from_legacy(config: LegacyConfig) -> Self {
        Self {
            additional_listeners: config
                .listeners
                .into_iter()
                .map(|(name, listener)| (name, AdditionalListenerConfig::from_legacy(listener)))
                .collect(),
            dns_overrides: config.dns,
            accounting: config.accounting,
            ..Default::default()
        }
    }
}

impl AdditionalListenerConfig {
    fn from_legacy(listener: Listener) -> Self {
        match listener.mode {
            ListenerMode::Passthrough | ListenerMode::Terminate => {
                let action = if listener.mode == ListenerMode::Passthrough {
                    TlsRouteAction::Passthrough {
                        target_port: listener.target_port,
                        target: listener.target,
                    }
                } else {
                    TlsRouteAction::Terminate {
                        target_port: listener.target_port,
                        target: listener.target,
                        upstream: if listener.upstream_tls {
                            UpstreamTransport::Tls
                        } else {
                            UpstreamTransport::Plaintext
                        },
                    }
                };
                Self::Tls(HostRoutedTlsListenerConfig {
                    bind: listener.bind,
                    routing: OrdinaryTlsConfig {
                        routes: Vec::new(),
                        unknown_host_policy: listener.policy,
                        unknown_host_rules: listener.rules,
                        default_route: action,
                        max_idle_time_ms: listener.max_idle_time_ms,
                        speed_limit: listener.speed_limit,
                    },
                })
            }
            ListenerMode::Http => Self::Http(HostRoutedHttpListenerConfig {
                bind: listener.bind,
                routes: Vec::new(),
                unknown_host_policy: listener.policy,
                unknown_host_rules: listener.rules,
                default_route: HttpRouteAction {
                    target_port: listener.target_port,
                    target: listener.target,
                    upstream: if listener.upstream_tls {
                        UpstreamTransport::Tls
                    } else {
                        UpstreamTransport::Plaintext
                    },
                },
                max_idle_time_ms: listener.max_idle_time_ms,
                speed_limit: listener.speed_limit,
            }),
            ListenerMode::Forward => Self::Forward(RawForwardListenerConfig {
                bind: listener.bind,
                targets: listener.target.unwrap_or_default(),
                upstream_tls: listener.upstream_tls,
                max_idle_time_ms: listener.max_idle_time_ms,
                speed_limit: listener.speed_limit,
            }),
        }
    }
}

fn bind_uses_port_443(bind: &str) -> bool {
    bind.rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        == Some(443)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_mandatory_443_and_deny_ordinary_sni() {
        let config = RuntimeConfig::default();
        assert_eq!(config.default_listener.bind, "0.0.0.0:443");
        assert_eq!(
            config.default_listener.ordinary_traffic.unknown_host_policy,
            Policy::ALLOW
        );
        assert!(config
            .default_listener
            .ordinary_traffic
            .unknown_host_rules
            .static_hosts
            .is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_non_443_default_bind() {
        let mut config = RuntimeConfig::default();
        config.default_listener.bind = "127.0.0.1:8443".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_malformed_public_dns_and_self_ip_settings() {
        let mut config = RuntimeConfig::default();
        config.control_plane.self_ips.insert("not-an-ip".into());
        assert!(config.validate().is_err());
        config.control_plane.self_ips.clear();
        config.control_plane.public_resolvers = vec!["resolver.example".into()];
        assert!(config.validate().is_err());
        config.control_plane.public_resolvers =
            vec!["1.1.1.1".into(), "[2606:4700:4700::1111]:53".into()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn json_is_forward_compatible() {
        let config: RuntimeConfig = serde_json::from_str(
            r#"{"control_plane":{"hostname":"tls.example"},"future_field":true}"#,
        )
        .unwrap();
        assert_eq!(config.control_plane.hostname, "tls.example");
        assert_eq!(config.default_listener.bind, "0.0.0.0:443");
    }

    #[test]
    fn host_matcher_respects_label_boundaries() {
        let matcher = HostMatcher {
            exact: vec!["exact.example".into()],
            suffix: vec![".internal.example".into()],
            patterns: vec![Regex::new("^api[0-9]+\\.example$").unwrap()],
        };
        assert!(matcher.matches("EXACT.EXAMPLE"));
        assert!(matcher.matches("a.internal.example"));
        assert!(matcher.matches("internal.example"));
        assert!(!matcher.matches("notinternal.example"));
        assert!(matcher.matches("api12.example"));
    }

    #[test]
    fn configured_routes_win_and_unknown_hosts_use_acl_default() {
        let mut ordinary = OrdinaryTlsConfig::default();
        ordinary.routes.push(TlsHostRoute {
            name: "terminated".into(),
            matcher: HostMatcher {
                exact: vec!["app.example".into()],
                ..Default::default()
            },
            action: TlsRouteAction::Terminate {
                target_port: 8080,
                target: None,
                upstream: UpstreamTransport::Plaintext,
            },
        });
        assert!(matches!(
            ordinary.select_route("app.example"),
            Some(TlsRouteAction::Terminate {
                upstream: UpstreamTransport::Plaintext,
                ..
            })
        ));
        assert!(ordinary.select_route("unknown.example").is_none());
        ordinary
            .unknown_host_rules
            .static_hosts
            .push("unknown.example".into());
        assert!(matches!(
            ordinary.select_route("unknown.example"),
            Some(TlsRouteAction::Passthrough { .. })
        ));
    }

    #[test]
    fn additional_tls_listener_has_per_host_modes() {
        let listener = AdditionalListenerConfig::Tls(HostRoutedTlsListenerConfig {
            bind: "127.0.0.1:9443".into(),
            routing: OrdinaryTlsConfig {
                routes: vec![TlsHostRoute {
                    name: "plaintext-app".into(),
                    matcher: HostMatcher {
                        exact: vec!["app.example".into()],
                        ..Default::default()
                    },
                    action: TlsRouteAction::Terminate {
                        target_port: 8080,
                        target: None,
                        upstream: UpstreamTransport::Plaintext,
                    },
                }],
                ..Default::default()
            },
        });
        let AdditionalListenerConfig::Tls(listener) = listener else {
            unreachable!()
        };
        assert!(matches!(
            listener.routing.select_route("app.example"),
            Some(TlsRouteAction::Terminate { .. })
        ));
    }
}
