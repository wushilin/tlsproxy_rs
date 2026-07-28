//! Public DNS prerequisite for ACME issuance.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hickory_resolver::config::{
    LookupIpStrategy, NameServerConfigGroup, ResolveHosts, ResolverConfig, ResolverOpts,
};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::TokioResolver;
use log::{info, warn};

use super::backend::{DnsFuture, DnsPrerequisite};
use crate::acme_types::ControlPlaneConfig;
use crate::acme_types::DnsDiagnostic;
use crate::store::normalize_domain;

#[derive(Clone)]
pub struct PublicDnsPrerequisite {
    query_timeout: Duration,
    store: Option<crate::store::Store>,
}

impl Default for PublicDnsPrerequisite {
    fn default() -> Self {
        Self {
            query_timeout: Duration::from_secs(5),
            store: None,
        }
    }
}

#[derive(Debug)]
struct ResolverResult {
    resolver: String,
    addresses: Result<BTreeSet<IpAddr>, String>,
}

impl PublicDnsPrerequisite {
    pub fn with_store(mut self, store: crate::store::Store) -> Self { self.store = Some(store); self }
    async fn lookup(&self, resolver: &str, domain: &str) -> ResolverResult {
        let result = async {
            let (ip, port) = parse_resolver(resolver)?;
            let nameservers = NameServerConfigGroup::from_ips_clear(&[ip], port, true);
            let config = ResolverConfig::from_parts(None, Vec::new(), nameservers);
            let mut options = ResolverOpts::default();
            options.timeout = self.query_timeout;
            options.attempts = 1;
            options.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
            options.use_hosts_file = ResolveHosts::Never;
            let resolver_client = TokioResolver::builder_with_config(
                config,
                TokioConnectionProvider::default(),
            )
            .with_options(options)
            .build();
            let lookup = resolver_client
                .lookup_ip(format!("{domain}."))
                .await
                .with_context(|| format!("DNS lookup failed via {resolver}"))?;
            Ok::<_, anyhow::Error>(lookup.iter().collect())
        }
        .await;
        ResolverResult {
            resolver: resolver.to_owned(),
            addresses: result.map_err(|cause| format!("{cause:#}")),
        }
    }

    async fn verify_inner(&self, certificate_id: &str, domains: &[String], control: &ControlPlaneConfig) -> Result<()> {
        if control.public_resolvers.is_empty() {
            bail!("at least one public DNS resolver must be configured");
        }
        let self_ips = control
            .self_ips
            .iter()
            .map(|value| value.parse::<IpAddr>())
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .context("configured self IP is invalid")?;
        if self_ips.is_empty() {
            bail!("at least one self IP must be configured before ACME issuance");
        }

        let mut diagnostics = Vec::new();
        for domain in domains {
            let domain = normalize_domain(domain)?;
            let results = futures_util::future::join_all(
                control
                    .public_resolvers
                    .iter()
                    .map(|resolver| self.lookup(resolver, &domain)),
            )
            .await;
            let evaluation = evaluate_domain(&domain, &self_ips, &results, control.require_all_resolvers);
            let checked_at = Some(time::OffsetDateTime::now_utc());
            diagnostics.extend(results.iter().map(|result| {
                let (addresses, error) = match &result.addresses { Ok(v) => (v.iter().map(ToString::to_string).collect(), None), Err(e) => (BTreeSet::new(), Some(e.clone())) };
                let passed = error.is_none() && !addresses.is_empty() && addresses.iter().all(|ip| ip.parse().is_ok_and(|ip| self_ips.contains(&ip)));
                DnsDiagnostic { certificate_id: certificate_id.into(), domain: domain.clone(), resolver: result.resolver.clone(), addresses, passed, error, checked_at }
            }));
            if let Err(cause) = evaluation {
                if let Some(store) = &self.store { store.save_dns_diagnostics(certificate_id, &diagnostics)?; }
                return Err(cause);
            }
            for result in results {
                match result.addresses {
                    Ok(addresses) => info!(
                        "ACME DNS prerequisite passed: domain={domain}, resolver={}, addresses={addresses:?}",
                        result.resolver
                    ),
                    Err(cause) => warn!(
                        "ACME DNS resolver failed but policy passed: domain={domain}, resolver={}, error={cause}",
                        result.resolver
                    ),
                }
            }
        }
        if let Some(store) = &self.store { store.save_dns_diagnostics(certificate_id, &diagnostics)?; }
        Ok(())
    }
}

impl DnsPrerequisite for PublicDnsPrerequisite {
    fn verify<'a>(
        &'a self,
        certificate_id: &'a str,
        domains: &'a [String],
        control: &'a ControlPlaneConfig,
    ) -> DnsFuture<'a> {
        Box::pin(async move { self.verify_inner(certificate_id, domains, control).await })
    }
}

pub(crate) fn parse_resolver(value: &str) -> Result<(IpAddr, u16)> {
    let value = value.trim();
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok((ip, 53));
    }
    let address = value
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid public DNS resolver `{value}`"))?;
    Ok((address.ip(), address.port()))
}

fn evaluate_domain(
    domain: &str,
    self_ips: &BTreeSet<IpAddr>,
    results: &[ResolverResult],
    require_all: bool,
) -> Result<()> {
    let passes = results
        .iter()
        .map(|result| match &result.addresses {
            Ok(addresses) => !addresses.is_empty() && addresses.is_subset(self_ips),
            Err(_) => false,
        })
        .collect::<Vec<_>>();
    let passed = if require_all {
        passes.iter().all(|passed| *passed)
    } else {
        passes.iter().any(|passed| *passed)
    };
    if passed {
        return Ok(());
    }
    let details = results
        .iter()
        .map(|result| match &result.addresses {
            Ok(addresses) => format!("{}={addresses:?}", result.resolver),
            Err(cause) => format!("{}=error({cause})", result.resolver),
        })
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "public DNS prerequisite failed for `{domain}`: expected only {self_ips:?}; {details}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(resolver: &str, addresses: &[&str]) -> ResolverResult {
        ResolverResult {
            resolver: resolver.into(),
            addresses: Ok(addresses.iter().map(|value| value.parse().unwrap()).collect()),
        }
    }

    #[test]
    fn parses_ipv4_ipv6_and_explicit_socket_resolvers() {
        assert_eq!(parse_resolver("1.1.1.1").unwrap().1, 53);
        assert_eq!(parse_resolver("2606:4700:4700::1111").unwrap().1, 53);
        assert_eq!(parse_resolver("127.0.0.1:5353").unwrap().1, 5353);
        assert!(parse_resolver("dns.example").is_err());
    }

    #[test]
    fn any_and_all_resolver_policies_are_distinct() {
        let self_ips = ["192.0.2.10".parse().unwrap()].into_iter().collect();
        let results = vec![
            result("one", &["192.0.2.10"]),
            result("two", &["192.0.2.99"]),
        ];
        assert!(evaluate_domain("app.example", &self_ips, &results, false).is_ok());
        assert!(evaluate_domain("app.example", &self_ips, &results, true).is_err());
    }

    #[test]
    fn mixed_self_and_foreign_answers_fail_to_avoid_random_ca_routing() {
        let self_ips = ["192.0.2.10".parse().unwrap()].into_iter().collect();
        let results = vec![result("one", &["192.0.2.10", "192.0.2.99"])];
        assert!(evaluate_domain("app.example", &self_ips, &results, false).is_err());
    }

    #[test]
    fn resolver_errors_fail_all_policy_but_can_be_tolerated_by_any_policy() {
        let self_ips = ["192.0.2.10".parse().unwrap()].into_iter().collect();
        let results = vec![
            result("one", &["192.0.2.10"]),
            ResolverResult {
                resolver: "two".into(),
                addresses: Err("timeout".into()),
            },
        ];
        assert!(evaluate_domain("app.example", &self_ips, &results, false).is_ok());
        assert!(evaluate_domain("app.example", &self_ips, &results, true).is_err());
    }
}
