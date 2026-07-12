use regex::Regex;
use serde_derive::{Deserialize, Serialize};
use serde_yaml_ng;
use std::error::Error;
use std::{collections::HashMap, path::Path};
use tokio::fs;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub listeners: HashMap<String, Listener>,
    #[serde(default)]
    pub options: Options,
    #[serde(default)]
    pub dns: DnsConfig,
    /// Certificate authority used for every certificate the proxy manages
    /// (admin server TLS and terminating listeners). When absent, a local CA
    /// with default parameters is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<CaConfig>,
    pub admin_server: Option<AdminServerConfig>,
    /// Accounting CDR log. Absent or `enabled: false` disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<AccountingConfig>,
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

/// Per-connection CDR accounting log with size-based rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub log_file: String,
    #[serde(default = "default_rotate_size")]
    pub rotate_size: String,
    #[serde(default = "default_max_keep")]
    pub max_keep: usize,
    /// Rotated indexes 1..=compress_after stay uncompressed.
    #[serde(default = "default_compress_after")]
    pub compress_after: usize,
    #[serde(default)]
    pub compression: Compression,
}

fn default_rotate_size() -> String {
    "100MiB".into()
}

fn default_max_keep() -> usize {
    10
}

fn default_compress_after() -> usize {
    3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    #[default]
    Zstd,
    Gzip,
    None,
}

impl Compression {
    pub fn extension(&self) -> Option<&'static str> {
        match self {
            Compression::Zstd => Some("zst"),
            Compression::Gzip => Some("gz"),
            Compression::None => None,
        }
    }
}

impl AccountingConfig {
    pub fn rotate_size_bytes(&self) -> Result<u64, String> {
        parse_size(&self.rotate_size)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.log_file.trim().is_empty() {
            return Err("accounting.log_file is required when accounting.enabled is true".into());
        }
        if self.max_keep == 0 {
            return Err("accounting.max_keep must be at least 1".into());
        }
        self.rotate_size_bytes()?;
        Ok(())
    }
}

/// Parses `123` (bytes), `2KiB`, `18MiB`, or `1GiB` (case-insensitive) into bytes.
pub fn parse_size(size: &str) -> Result<u64, String> {
    let lower = size.trim().to_ascii_lowercase();
    let (digits, multiplier) = if let Some(digits) = lower.strip_suffix("kib") {
        (digits, 1024u64)
    } else if let Some(digits) = lower.strip_suffix("mib") {
        (digits, 1024 * 1024)
    } else if let Some(digits) = lower.strip_suffix("gib") {
        (digits, 1024 * 1024 * 1024)
    } else {
        (lower.as_str(), 1)
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid size `{size}`; use bytes or a KiB/MiB/GiB suffix"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size `{size}` overflows"))
}

impl Config {
    pub async fn load_file<P: AsRef<Path>>(filename: P) -> Result<Config, Box<dyn Error>> {
        let content = fs::read_to_string(filename).await?;
        Self::load_string(&content)
    }

    pub fn load_string(content: &str) -> Result<Config, Box<dyn Error>> {
        let config: Config = serde_yaml_ng::from_str(content)?;
        if let Some(accounting) = &config.accounting {
            accounting.validate()?;
        }
        return Ok(config);
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
options: {}
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
options: {}
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
options: {}
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
options: {}
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

    #[test]
    fn parse_size_accepts_bytes_and_binary_suffixes() {
        use super::parse_size;
        assert_eq!(parse_size("123").unwrap(), 123);
        assert_eq!(parse_size("2KiB").unwrap(), 2 * 1024);
        assert_eq!(parse_size("18MiB").unwrap(), 18 * 1024 * 1024);
        assert_eq!(parse_size("1GiB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 10 MiB ").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("18mib").unwrap(), 18 * 1024 * 1024);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10MB").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn accounting_block_parses_with_defaults() {
        let yaml = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {}
dns: {}
accounting:
  enabled: true
  log_file: cdr.log
"#;
        let config = Config::load_string(yaml).expect("accounting configuration should parse");
        let accounting = config.accounting.expect("accounting block should be present");
        assert!(accounting.enabled);
        assert_eq!(accounting.log_file, "cdr.log");
        assert_eq!(accounting.rotate_size, "100MiB");
        assert_eq!(accounting.rotate_size_bytes().unwrap(), 100 * 1024 * 1024);
        assert_eq!(accounting.max_keep, 10);
        assert_eq!(accounting.compress_after, 3);
        assert_eq!(accounting.compression, super::Compression::Zstd);
    }

    #[test]
    fn accounting_absent_is_none_and_old_configs_still_load() {
        let yaml = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {}
dns: {}
"#;
        let config = Config::load_string(yaml).expect("config without accounting should parse");
        assert!(config.accounting.is_none());
    }

    #[test]
    fn accounting_enabled_requires_log_file_and_valid_rotate_size() {
        let base = |extra: &str| {
            format!(
                r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {{}}
dns: {{}}
accounting:
  enabled: true
{extra}
"#
            )
        };
        assert!(Config::load_string(&base("")).is_err(), "missing log_file must fail");
        assert!(
            Config::load_string(&base("  log_file: cdr.log\n  rotate_size: banana")).is_err(),
            "invalid rotate_size must fail"
        );
        assert!(
            Config::load_string(&base("  log_file: cdr.log\n  max_keep: 0")).is_err(),
            "max_keep 0 must fail"
        );
        assert!(
            Config::load_string(&base("  log_file: cdr.log\n  compression: gzip")).is_ok(),
            "gzip compression must parse"
        );
        // disabled block with nonsense values loads fine (never validated)
        let disabled = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {}
dns: {}
accounting:
  enabled: false
"#;
        assert!(Config::load_string(disabled).is_ok());
    }
}
