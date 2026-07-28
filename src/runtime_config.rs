use std::collections::{HashMap, HashSet};

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
    /// Additional listeners intentionally stopped from the control plane.
    /// Their configuration is retained so they can be started again.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub disabled_listeners: HashSet<String>,
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
            disabled_listeners: HashSet::new(),
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
    /// Plain HTTP listener whose only host action is an HTTPS redirect.
    Redirect(HostRoutedHttpListenerConfig),
    /// Raw byte forwarding has no hostname and therefore no host route table.
    Forward(RawForwardListenerConfig),
}

impl AdditionalListenerConfig {
    pub fn bind(&self) -> &str {
        match self {
            Self::Tls(config) => &config.bind,
            Self::Http(config) => &config.bind,
            Self::Redirect(config) => &config.bind,
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
    /// Default destination port used by HTTPS redirect actions.
    #[serde(default = "default_target_port")]
    pub redirect_https_port: u16,
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
        if let Some(route) = best_host_route(&self.routes, host, |route| &route.matcher) {
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
    #[serde(default)]
    pub behavior: HttpBehavior,
    #[serde(default = "default_http_target_port")]
    pub target_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub upstream: UpstreamTransport,
    /// Preferred backend pool. Empty preserves the legacy target/port shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<HttpBackend>,
    #[serde(default)]
    pub load_balancing: HttpLoadBalancing,
    /// Optional Host header sent upstream. Upstream TLS SNI remains backend-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_header: Option<String>,
    /// Optional per-request path routes. Longest matching segment prefix wins;
    /// the fields above remain the host's default reverse-proxy action.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<HttpPathRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<HttpRedirectConfig>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpBehavior { #[default] ReverseProxy, RedirectHttps }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRedirectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default = "default_redirect_status")]
    pub status: u16,
    #[serde(default)]
    pub preserve_host: bool,
}

fn default_redirect_status() -> u16 { 308 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpPathRoute {
    pub prefix: String,
    #[serde(default)]
    pub strip_prefix: bool,
    #[serde(default)]
    pub basic_auth: HttpBasicAuth,
    pub action: HttpPathAction,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HttpBasicAuth {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub users: Vec<HttpBasicAuthUser>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpBasicAuthUser { pub username: String, pub password: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HttpPathAction {
    ReverseProxy {
        #[serde(flatten)]
        action: HttpRouteAction,
    },
    StaticFiles {
        document_root: String,
        #[serde(default = "default_index_file")]
        index: Option<String>,
        #[serde(default)]
        directory_listing: bool,
    },
}

fn default_index_file() -> Option<String> { Some("index.html".into()) }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpBackend {
    pub address: String,
    #[serde(default)]
    pub transport: UpstreamTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_server_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpLoadBalancing {
    #[default]
    RoundRobin,
    ClientIpHash,
}

impl Default for HttpRouteAction {
    fn default() -> Self {
        Self {
            target_port: default_http_target_port(),
            target: None,
            upstream: UpstreamTransport::Plaintext,
            backends: Vec::new(),
            load_balancing: HttpLoadBalancing::RoundRobin,
            host_header: None,
            paths: Vec::new(),
            behavior: HttpBehavior::ReverseProxy,
            redirect: None,
        }
    }
}

impl HttpRouteAction {
    pub fn select_path(&self, path: &str) -> Option<&HttpPathRoute> {
        let path = path.split(['?', '#']).next().unwrap_or(path);
        self.paths.iter().filter(|route| path_prefix_matches(&route.prefix, path))
            .max_by_key(|route| route.prefix.trim_end_matches('/').len())
    }
}

fn path_prefix_matches(prefix: &str, path: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty() || path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
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
    #[serde(default)]
    pub load_balancing: HttpLoadBalancing,
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
        if let Some(route) = best_host_route(&self.routes, host, |route| &route.matcher) {
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
        self.match_score(host).is_some()
    }

    /// Higher scores win: exact, then longest suffix, then regex. Regex
    /// routes deliberately share one score so their configuration order is
    /// the only precedence rule.
    fn match_score(&self, host: &str) -> Option<(u8, usize)> {
        let host = host.trim_end_matches('.');
        if self.exact.iter().any(|entry| host.eq_ignore_ascii_case(entry.trim_end_matches('.'))) {
            return Some((3, host.len()));
        }
        let suffix = self.suffix.iter().filter_map(|entry| {
                let suffix = entry.trim().trim_start_matches('.').trim_end_matches('.');
                (host.eq_ignore_ascii_case(suffix)
                    || host.len() > suffix.len()
                        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
                        && host[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix))
                    .then_some(suffix.len())
            }).max();
        if let Some(length) = suffix { return Some((2, length)); }
        self.patterns.iter().any(|pattern| pattern.is_match(host)).then_some((1, 0))
    }
}

fn best_host_route<'a, T, F>(routes: &'a [T], host: &str, matcher: F) -> Option<&'a T>
where F: Fn(&T) -> &HostMatcher {
    let mut best: Option<(&T, (u8, usize))> = None;
    for route in routes {
        if let Some(score) = matcher(route).match_score(host) {
            if best.as_ref().is_none_or(|(_, current)| score > *current) {
                best = Some((route, score));
            }
        }
    }
    best.map(|(route, _)| route)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TlsRouteAction {
    Passthrough {
        #[serde(default = "default_target_port")]
        target_port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default)]
        load_balancing: HttpLoadBalancing,
    },
    Terminate {
        #[serde(default = "default_target_port")]
        target_port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default)]
        upstream: UpstreamTransport,
        #[serde(default)]
        load_balancing: HttpLoadBalancing,
    },
    /// Terminate client TLS, parse HTTP/1.1, and proxy using a Layer-7 backend
    /// pool. This allows HTTPS reverse proxying to coexist with encrypted TLS
    /// passthrough and generic TCP termination on the same socket.
    ReverseProxy {
        #[serde(flatten)]
        action: HttpRouteAction,
    },
    Reject,
}

impl Default for TlsRouteAction {
    fn default() -> Self {
        Self::Passthrough {
            target_port: default_target_port(),
            target: None,
            load_balancing: HttpLoadBalancing::RoundRobin,
        }
    }
}

impl TlsRouteAction {
    pub fn target_port(&self) -> Option<u16> {
        match self {
            Self::Passthrough { target_port, .. }
            | Self::Terminate { target_port, .. } => Some(*target_port),
            Self::ReverseProxy { .. } => None,
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
            if let TlsRouteAction::ReverseProxy { action } = route {
                if action.behavior != HttpBehavior::ReverseProxy { bail!("default TLS listener cannot use an HTTP redirect action"); }
                validate_http_action("default listener", action)?;
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
                        if let TlsRouteAction::ReverseProxy { action } = action {
                            if action.behavior != HttpBehavior::ReverseProxy { bail!("listener `{name}` TLS route cannot use an HTTP redirect action"); }
                            validate_http_action(&format!("listener `{name}`"), action)?;
                        }
                    }
                }
                AdditionalListenerConfig::Http(listener) => {
                    for action in listener.routes.iter().map(|route| &route.action).chain(std::iter::once(&listener.default_route)) {
                        if action.behavior != HttpBehavior::ReverseProxy { bail!("listener `{name}` HTTP reverse-proxy routes cannot redirect; use a redirect listener"); }
                    }
                    if listener.redirect_https_port == 0 { bail!("listener `{name}` HTTPS redirect port must be non-zero"); }
                    if listener.default_route.behavior == HttpBehavior::RedirectHttps {
                        let redirect = listener.default_route.redirect.as_ref();
                        if redirect.and_then(|value| value.hostname.as_deref()).is_none()
                            && !redirect.is_some_and(|value| value.preserve_host)
                        {
                            bail!("listener `{name}` Anything else HTTPS redirect requires a fixed hostname or preserve_host=true");
                        }
                    }
                    if listener
                        .routes
                        .iter()
                        .map(|route| route.action.target_port)
                        .chain(std::iter::once(listener.default_route.target_port))
                        .any(|port| port == 0)
                    {
                        bail!("listener `{name}` HTTP route target port must be non-zero");
                    }
                    for action in listener.routes.iter().map(|route| &route.action).chain(std::iter::once(&listener.default_route)) {
                        validate_http_action(&format!("listener `{name}`"), action)?;
                    }
                }
                AdditionalListenerConfig::Redirect(listener) => {
                    if listener.redirect_https_port == 0 { bail!("listener `{name}` HTTPS redirect port must be non-zero"); }
                    for action in listener.routes.iter().map(|route| &route.action).chain(std::iter::once(&listener.default_route)) {
                        if action.behavior != HttpBehavior::RedirectHttps { bail!("listener `{name}` redirect routes must use HTTPS redirect behavior"); }
                        validate_http_action(&format!("listener `{name}`"), action)?;
                    }
                    let redirect = listener.default_route.redirect.as_ref();
                    if redirect.and_then(|value| value.hostname.as_deref()).is_none() && !redirect.is_some_and(|value| value.preserve_host) {
                        bail!("listener `{name}` Anything else HTTPS redirect requires a fixed hostname or preserve_host=true");
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

fn validate_http_action(scope: &str, action: &HttpRouteAction) -> Result<()> {
    if action.behavior == HttpBehavior::RedirectHttps {
        let redirect = action.redirect.as_ref();
        if redirect.is_some_and(|value| !matches!(value.status, 301 | 308)) { bail!("{scope} HTTPS redirect status must be 301 or 308"); }
        if redirect.and_then(|value| value.port).is_some_and(|port| port == 0) { bail!("{scope} HTTPS redirect port must be non-zero"); }
        if let Some(hostname) = redirect.and_then(|value| value.hostname.as_deref()) { crate::store::normalize_domain(hostname)?; }
        return Ok(());
    }
    for backend in &action.backends {
        let parsed: crate::hostutil::HostAndPort = backend.address.parse()
            .map_err(|cause| anyhow::anyhow!("{scope} HTTP backend `{}` is invalid: {cause}", backend.address))?;
        if parsed.port() == 0 {
            bail!("{scope} HTTP backend port must be non-zero");
        }
    }
    let mut prefixes = std::collections::HashSet::new();
    for route in &action.paths {
        if !route.prefix.starts_with('/') || route.prefix.contains('?') || route.prefix.contains('#') {
            bail!("{scope} HTTP path prefix `{}` must start with `/` and contain no query or fragment", route.prefix);
        }
        let normalized = if route.prefix == "/" { "/" } else { route.prefix.trim_end_matches('/') };
        if !prefixes.insert(normalized.to_string()) { bail!("{scope} has duplicate HTTP path prefix `{normalized}`"); }
        if route.basic_auth.enabled {
            if route.basic_auth.users.is_empty() { bail!("{scope} HTTP path `{}` enables Basic Auth without users", route.prefix); }
            let mut usernames = std::collections::HashSet::new();
            for user in &route.basic_auth.users {
                if user.username.is_empty() || user.username.contains(':') || user.username.contains(['\r', '\n']) { bail!("{scope} HTTP Basic Auth username is invalid"); }
                if user.password.contains(['\r', '\n']) { bail!("{scope} HTTP Basic Auth password is invalid"); }
                if !usernames.insert(&user.username) { bail!("{scope} HTTP path `{}` has duplicate Basic Auth username `{}`", route.prefix, user.username); }
            }
        }
        match &route.action {
            HttpPathAction::ReverseProxy { action } => {
                if action.behavior != HttpBehavior::ReverseProxy {
                    bail!("{scope} HTTP path `{}` cannot use an HTTPS redirect action; use a redirect listener", route.prefix);
                }
                validate_http_action(scope, action)?
            }
            HttpPathAction::StaticFiles { document_root, index, .. } => {
                let root = std::fs::canonicalize(document_root)
                    .map_err(|cause| anyhow::anyhow!("{scope} static document root `{document_root}` is invalid: {cause}"))?;
                if !root.is_dir() { bail!("{scope} static document root `{document_root}` is not a directory"); }
                if index.as_deref().is_some_and(|name| name.contains(['/', '\\']) || name == "..") {
                    bail!("{scope} static index must be a filename within the requested directory");
                }
            }
        }
    }
    Ok(())
}

impl AdditionalListenerConfig {
    fn from_legacy(listener: Listener) -> Self {
        match listener.mode {
            ListenerMode::Passthrough | ListenerMode::Terminate => {
                let action = if listener.mode == ListenerMode::Passthrough {
                    TlsRouteAction::Passthrough {
                        target_port: listener.target_port,
                        target: listener.target,
                        load_balancing: HttpLoadBalancing::RoundRobin,
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
                        load_balancing: HttpLoadBalancing::RoundRobin,
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
                redirect_https_port: 443,
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
                    backends: Vec::new(),
                    load_balancing: HttpLoadBalancing::RoundRobin,
                    host_header: None,
                    paths: Vec::new(),
                    behavior: HttpBehavior::ReverseProxy,
                    redirect: None,
                },
                max_idle_time_ms: listener.max_idle_time_ms,
                speed_limit: listener.speed_limit,
            }),
            ListenerMode::Forward => Self::Forward(RawForwardListenerConfig {
                bind: listener.bind,
                targets: listener.target.unwrap_or_default(),
                upstream_tls: listener.upstream_tls,
                load_balancing: HttpLoadBalancing::RoundRobin,
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
    fn host_routes_use_exact_suffix_regex_then_default_precedence() {
        let action = |port| TlsRouteAction::Passthrough { target_port: port, target: None, load_balancing: HttpLoadBalancing::RoundRobin };
        let routing = OrdinaryTlsConfig {
            routes: vec![
                TlsHostRoute { name: String::new(), matcher: HostMatcher { patterns: vec![Regex::new("example").unwrap()], ..Default::default() }, action: action(1001) },
                TlsHostRoute { name: String::new(), matcher: HostMatcher { suffix: vec!["example.com".into()], ..Default::default() }, action: action(1002) },
                TlsHostRoute { name: String::new(), matcher: HostMatcher { suffix: vec!["deep.example.com".into()], ..Default::default() }, action: action(1003) },
                TlsHostRoute { name: String::new(), matcher: HostMatcher { exact: vec!["api.deep.example.com".into()], ..Default::default() }, action: action(1004) },
            ],
            unknown_host_policy: Policy::DENY,
            unknown_host_rules: empty_rules(),
            default_route: action(1005),
            max_idle_time_ms: None,
            speed_limit: None,
        };
        assert_eq!(routing.select_route("API.DEEP.EXAMPLE.COM.").and_then(TlsRouteAction::target_port), Some(1004));
        assert_eq!(routing.select_route("other.deep.example.com").and_then(TlsRouteAction::target_port), Some(1003));
        assert_eq!(routing.select_route("other.example.com").and_then(TlsRouteAction::target_port), Some(1002));
        assert_eq!(routing.select_route("example.net").and_then(TlsRouteAction::target_port), Some(1001));
        assert_eq!(routing.select_route("unmatched.test").and_then(TlsRouteAction::target_port), Some(1005));
    }

    #[test]
    fn regex_host_routes_use_configuration_order() {
        let action = |port| TlsRouteAction::Passthrough { target_port: port, target: None, load_balancing: HttpLoadBalancing::RoundRobin };
        let routing = OrdinaryTlsConfig {
            routes: vec![
                TlsHostRoute { name: "first".into(), matcher: HostMatcher { patterns: vec![Regex::new("^api.*\\.example$").unwrap()], ..Default::default() }, action: action(1001) },
                TlsHostRoute { name: "second".into(), matcher: HostMatcher { patterns: vec![Regex::new("^api-[a-z]+\\.example$").unwrap()], ..Default::default() }, action: action(1002) },
            ],
            ..Default::default()
        };
        assert_eq!(routing.select_route("api-test.example").and_then(TlsRouteAction::target_port), Some(1001));
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
                load_balancing: HttpLoadBalancing::RoundRobin,
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
                        load_balancing: HttpLoadBalancing::RoundRobin,
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

    #[test]
    fn tls_reverse_proxy_action_round_trips_and_validates() {
        let json = r#"{
            "mode":"reverse_proxy",
            "backends":[
                {"address":"app.internal:8080","transport":"plaintext"},
                {"address":"secure.internal:8443","transport":"tls","tls_server_name":"secure.example"}
            ],
            "load_balancing":"client_ip_hash",
            "host_header":"app.internal"
        }"#;
        let tls_action: TlsRouteAction = serde_json::from_str(json).unwrap();
        let TlsRouteAction::ReverseProxy { action } = &tls_action else { panic!("wrong action") };
        assert_eq!(action.backends.len(), 2);
        assert_eq!(action.load_balancing, HttpLoadBalancing::ClientIpHash);
        assert_eq!(serde_json::to_value(&tls_action).unwrap()["mode"], "reverse_proxy");

        let mut config = RuntimeConfig::default();
        config.default_listener.ordinary_traffic.routes.push(TlsHostRoute {
            name: "web".into(),
            matcher: HostMatcher { exact: vec!["web.example".into()], ..Default::default() },
            action: tls_action,
        });
        assert!(config.validate().is_ok());
    }

    #[test]
    fn http_path_routes_round_trip_and_longest_prefix_wins() {
        let json = r#"{
          "target_port":80,
          "paths":[
            {"prefix":"/","action":{"type":"static_files","document_root":"/srv/www","index":"index.html","directory_listing":false}},
            {"prefix":"/app","strip_prefix":true,"action":{"type":"reverse_proxy","backends":[{"address":"app:8080"}]}},
            {"prefix":"/app/admin","action":{"type":"reverse_proxy","backends":[{"address":"admin:8080"}]}}
          ]
        }"#;
        let action: HttpRouteAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.select_path("/app/admin/users").unwrap().prefix, "/app/admin");
        assert_eq!(action.select_path("/app/users").unwrap().prefix, "/app");
        assert_eq!(action.select_path("/assets/site.css").unwrap().prefix, "/");
        let encoded = serde_json::to_string(&action).unwrap();
        let decoded: HttpRouteAction = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.paths.len(), 3);
    }

    #[test]
    fn redirect_is_a_separate_listener_protocol() {
        let mut config: RuntimeConfig = serde_json::from_str(r#"{
          "additional_listeners": {
            "redirect": {
              "protocol":"redirect", "bind":"127.0.0.1:8080", "redirect_https_port":443,
              "unknown_host_policy":"DENY", "unknown_host_rules":{"static_hosts":[],"patterns":[]},
              "default_route":{"behavior":"redirect_https","redirect":{"status":308,"preserve_host":true}}
            }
          }
        }"#).unwrap();
        assert!(matches!(config.additional_listeners.get("redirect"), Some(AdditionalListenerConfig::Redirect(_))));
        assert!(config.validate().is_ok());

        let AdditionalListenerConfig::Redirect(listener) = config.additional_listeners.remove("redirect").unwrap() else { unreachable!() };
        config.additional_listeners.insert("wrong-kind".into(), AdditionalListenerConfig::Http(listener));
        assert!(config.validate().unwrap_err().to_string().contains("use a redirect listener"));
    }
}
