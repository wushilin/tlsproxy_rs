use regex::Regex;
use serde_derive::{Deserialize, Serialize};
use serde_yaml_ng;
use std::error::Error;
use std::{collections::HashMap, path::Path};
use tokio::fs;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub listeners: HashMap<String, Listener>,
    pub options: Options,
    #[serde(default)]
    pub dns: DnsConfig,
    /// Certificate authority used for every certificate the proxy manages
    /// (admin server TLS and terminating listeners). When absent, a local CA
    /// with default parameters is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<CaConfig>,
    pub admin_server: Option<AdminServerConfig>,
}

/// Local CA configuration. When absent, `localca` defaults are used.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localca: Option<LocalCaConfig>,
}

#[derive(Debug, Clone, PartialEq, Hash, Serialize, Deserialize)]
pub struct LocalCaConfig {
    #[serde(default = "default_ca_cert")]
    pub ca_cert: String,
    #[serde(default = "default_ca_key")]
    pub ca_key: String,
    #[serde(default = "default_ca_working_dir")]
    pub working_dir: String,
}

impl Default for LocalCaConfig {
    fn default() -> Self {
        Self {
            ca_cert: default_ca_cert(),
            ca_key: default_ca_key(),
            working_dir: default_ca_working_dir(),
        }
    }
}

fn default_ca_cert() -> String {
    "local_ca/CA.pem".into()
}

fn default_ca_key() -> String {
    "local_ca/CA.key".into()
}

fn default_ca_working_dir() -> String {
    "local_ca".into()
}

/// DNS overrides, applied before connecting upstream. Resolution priority:
/// exact host:port, then exact host (any port), then suffix rules with a
/// port, then suffix rules without (longer suffixes win within each group),
/// then regex rules in definition order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DnsConfig {
    #[serde(default)]
    pub exact: Vec<ExactDnsRule>,
    #[serde(default)]
    pub suffix: Vec<SuffixDnsRule>,
    #[serde(default)]
    pub regex: Vec<RegexDnsRule>,
}

/// `from` is `host` (any port) or `host:port`; `to` is `host` (keep original
/// port) or `host:port`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExactDnsRule {
    pub from: String,
    pub to: String,
}

/// `from` is a domain suffix, conventionally written with a leading dot
/// (`.internal.example.com`), optionally with `:port`. Matching respects
/// label boundaries: `.abc.com` matches `x.abc.com` and `abc.com`, but never
/// `notabc.com`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuffixDnsRule {
    pub from: String,
    pub to: String,
}

/// `hostname` is matched against the hostname only (the port is never part of
/// the text the regex sees); `port` optionally restricts the rule to one port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegexDnsRule {
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub to: String,
}

impl Listener {
    pub fn speed_limit(&self) -> f64 {
        match self.speed_limit.as_ref() {
            Some(inner) => {
                if *inner == 0f64 {
                    f64::INFINITY
                } else {
                    *inner
                }
            }
            None => f64::INFINITY,
        }
    }
    pub fn max_idle_time_ms(&self) -> u64 {
        match self.max_idle_time_ms.as_ref() {
            Some(inner) => {
                if *inner == 0 {
                    return u64::MAX;
                } else {
                    return *inner;
                }
            }
            None => {
                return 3600000;
            }
        }
    }

    fn match_host(&self, host: &str) -> bool {
        for static_host in &self.rules.static_hosts {
            if host.eq_ignore_ascii_case(static_host) {
                return true;
            }
        }

        for next_regex in &self.rules.patterns {
            if next_regex.is_match(host) {
                return true;
            }
        }
        return false;
    }
    pub fn is_allowed(&self, host: &str) -> bool {
        let matched = self.match_host(host);
        match self.policy {
            Policy::ALLOW => matched,
            Policy::DENY => !matched,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Hash, Serialize, Deserialize)]
pub struct AdminServerConfig {
    pub bind_address: Option<String>,
    pub bind_port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub tls: Option<bool>,
    /// Subject alternative names for the admin certificate; DNS names and
    /// IP addresses are told apart automatically. The certificate itself
    /// comes from the top-level `ca` source.
    #[serde(default)]
    pub san: Vec<String>,
}

impl Default for AdminServerConfig {
    fn default() -> Self {
        AdminServerConfig {
            bind_address: Some("0.0.0.0".into()),
            bind_port: Some(48888),
            username: Some("admin".into()),
            password: Some("admin".into()),
            tls: Some(false),
            san: vec!["localhost".into(), "127.0.0.1".into()],
        }
    }
}
impl Config {
    pub async fn load_file<P: AsRef<Path>>(filename: P) -> Result<Config, Box<dyn Error>> {
        let content = fs::read_to_string(filename).await?;

        let config: Config = serde_yaml_ng::from_str(&content)?;
        return Ok(config);
    }

    pub fn load_string(content: &str) -> Result<Config, Box<dyn Error>> {
        let config: Config = serde_yaml_ng::from_str(content)?;
        return Ok(config);
    }

    pub fn init_logging(&self) {
        let log_conf_file = &self.options.log_config_file;
        if log_conf_file.is_empty() {
            println!("not initing logging as no `log4rs.yaml` defined.");
        } else {
            let result = log4rs::init_file(log_conf_file, Default::default());
            match result {
                Err(cause) => {
                    println!("failed to initialize logging from `{log_conf_file}`: {cause}");
                }
                Ok(_) => {
                    println!("initialized logging from `{log_conf_file}`");
                }
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listener {
    pub bind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub target_port: u16,
    pub policy: Policy,
    pub rules: Rules,
    pub max_idle_time_ms: Option<u64>,
    pub speed_limit: Option<f64>,
    #[serde(default)]
    pub mode: ListenerMode,
    /// Encrypt the terminate-mode upstream leg without authenticating the
    /// upstream certificate.
    #[serde(default)]
    pub upstream_tls: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ListenerMode {
    #[default]
    Passthrough,
    Terminate,
    Forward,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rules {
    pub static_hosts: Vec<String>,
    #[serde(with = "ci_regex")]
    pub patterns: Vec<Regex>,
}

/// (De)serializes listener patterns as strings, compiling them
/// case-insensitively so hostnames match regardless of case (SNI hostnames
/// are matched the same way as `static_hosts`). The pattern string is stored
/// unchanged, so configs round-trip without gaining an implicit `(?i)`.
mod ci_regex {
    use regex::{Regex, RegexBuilder};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(patterns: &[Regex], serializer: S) -> Result<S::Ok, S::Error> {
        patterns
            .iter()
            .map(Regex::as_str)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Regex>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .iter()
            .map(|pattern| {
                RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map_err(|cause| {
                        serde::de::Error::custom(format!("invalid regex `{pattern}`: {cause}"))
                    })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Policy {
    ALLOW,
    DENY,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Options {
    pub log_config_file: String,
    /// Directory for runtime artifacts such as the local CA.
    #[serde(default = "default_runtime_dir")]
    pub runtime_dir: String,
}

fn default_runtime_dir() -> String {
    "./runtime".into()
}

impl Default for Options {
    fn default() -> Self {
        Self {
            log_config_file: "".into(),
            runtime_dir: default_runtime_dir(),
        }
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::{Config, Listener, ListenerMode, Policy, Rules};

    #[test]
    fn old_listener_config_defaults_to_passthrough() {
        let yaml = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options:
  log_config_file: ""
  self_ips: []
dns: {}
"#;
        let config = Config::load_string(yaml).expect("old configuration should deserialize");
        assert_eq!(config.listeners["web"].mode, ListenerMode::Passthrough);
        assert!(!config.listeners["web"].upstream_tls);
    }

    #[test]
    fn listener_mode_is_lowercase_in_yaml() {
        let yaml = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 8080
    policy: ALLOW
    rules:
      static_hosts: [example.test]
      patterns: []
    mode: terminate
    upstream_tls: true
options:
  log_config_file: ""
  self_ips: []
dns: {}
"#;
        let config = Config::load_string(yaml).expect("termination configuration should parse");
        assert_eq!(config.listeners["web"].mode, ListenerMode::Terminate);
        assert!(config.listeners["web"].upstream_tls);
    }

    #[test]
    fn forward_listener_accepts_target_list() {
        let yaml = r#"
listeners:
  plain:
    bind: 127.0.0.1:18080
    target: target1.host.com:80; target2.host.com:1080
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
    mode: forward
options:
  log_config_file: ""
dns: {}
"#;
        let config = Config::load_string(yaml).expect("forward configuration should parse");
        assert_eq!(config.listeners["plain"].mode, ListenerMode::Forward);
        assert_eq!(
            config.listeners["plain"].target.as_deref(),
            Some("target1.host.com:80; target2.host.com:1080")
        );
    }

    fn listener(policy: Policy) -> Listener {
        Listener {
            bind: "127.0.0.1:0".into(),
            target: None,
            target_port: 443,
            policy,
            rules: Rules {
                static_hosts: vec!["Example.COM".into()],
                patterns: vec![Regex::new(r"^api\d+\.example$").unwrap()],
            },
            max_idle_time_ms: None,
            speed_limit: None,
            mode: ListenerMode::Passthrough,
            upstream_tls: false,
        }
    }

    #[test]
    fn listener_patterns_match_case_insensitively() {
        let yaml = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: ALLOW
    rules:
      static_hosts: []
      patterns: ['^api\d+\.example$', '(?i)^legacy\.example$']
options:
  log_config_file: ""
dns: {}
"#;
        let config = Config::load_string(yaml).expect("pattern configuration should parse");
        let web = &config.listeners["web"];
        assert!(web.is_allowed("API7.EXAMPLE"));
        assert!(web.is_allowed("Legacy.Example"));
        assert!(!web.is_allowed("other.example"));

        let yaml_out = serde_yaml_ng::to_string(&config).expect("configuration should serialize");
        assert!(yaml_out.contains(r"^api\d+\.example$"));
        assert!(!yaml_out.contains(r"(?i)^api"));
    }

    #[test]
    fn acl_static_hosts_ignore_case_and_regexes_are_applied() {
        let allow = listener(Policy::ALLOW);
        assert!(allow.is_allowed("example.com"));
        assert!(allow.is_allowed("api42.example"));
        assert!(!allow.is_allowed("other.example"));

        let deny = listener(Policy::DENY);
        assert!(!deny.is_allowed("EXAMPLE.COM"));
        assert!(!deny.is_allowed("api7.example"));
        assert!(deny.is_allowed("other.example"));
    }
}
