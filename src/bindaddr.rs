use anyhow::{bail, Result};
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindPattern {
    Literal(String),
    Interface {
        name: String,
        family: Family,
        port: Option<u16>,
    },
}

/// Parses a bind value. `%eth0%:442` / `%eth0/v4%:442` / `%eth0/v6%:442`
/// select an interface; anything not starting with `%` is a literal.
/// A pattern without `:port` (admin `bind_address`) is also accepted.
pub fn parse_bind_pattern(input: &str) -> Result<BindPattern> {
    let trimmed = input.trim();
    if !trimmed.starts_with('%') {
        return Ok(BindPattern::Literal(trimmed.to_string()));
    }
    let rest = &trimmed[1..];
    let Some(end) = rest.find('%') else {
        bail!("invalid bind pattern `{input}`: missing closing `%`");
    };
    let inner = &rest[..end];
    let after = &rest[end + 1..];
    let (name, family) = match inner.split_once('/') {
        None => (inner, Family::V4),
        Some((name, "v4")) => (name, Family::V4),
        Some((name, "v6")) => (name, Family::V6),
        Some((_, other)) => {
            bail!("invalid bind pattern `{input}`: unknown family suffix `/{other}` (use /v4 or /v6)")
        }
    };
    if name.is_empty() {
        bail!("invalid bind pattern `{input}`: empty interface name");
    }
    let port = if after.is_empty() {
        None
    } else if let Some(port) = after.strip_prefix(':') {
        let port = port.parse::<u16>().map_err(|_| {
            anyhow::anyhow!("invalid bind pattern `{input}`: invalid port `{port}`")
        })?;
        Some(port)
    } else {
        bail!("invalid bind pattern `{input}`: expected `:port` after closing `%`");
    };
    Ok(BindPattern::Interface {
        name: name.to_string(),
        family,
        port,
    })
}

/// Parses, then resolves an interface pattern to a bindable address string
/// (`ip:port`, `[v6]:port`, or bare IP when the pattern has no port).
/// Literals are returned unchanged.
pub fn resolve_bind_addr(input: &str) -> Result<String> {
    let (name, family, port) = match parse_bind_pattern(input)? {
        BindPattern::Literal(literal) => return Ok(literal),
        BindPattern::Interface { name, family, port } => (name, family, port),
    };
    let interfaces = if_addrs::get_if_addrs()?;
    let matching: Vec<_> = interfaces
        .iter()
        .filter(|interface| interface.name == name)
        .collect();
    if matching.is_empty() {
        let mut names: Vec<&str> = interfaces
            .iter()
            .map(|interface| interface.name.as_str())
            .collect();
        names.sort();
        names.dedup();
        bail!(
            "interface \"{name}\" not found; available interfaces: {}",
            names.join(", ")
        );
    }
    let ip = matching
        .iter()
        .map(|interface| interface.ip())
        .find(|ip| match (family, ip) {
            (Family::V4, IpAddr::V4(_)) => true,
            // Skip link-local (fe80::/10); a stable address is wanted for binding.
            (Family::V6, IpAddr::V6(v6)) => (v6.segments()[0] & 0xffc0) != 0xfe80,
            _ => false,
        });
    let Some(ip) = ip else {
        let addresses: Vec<String> = matching
            .iter()
            .map(|interface| interface.ip().to_string())
            .collect();
        let wanted = match family {
            Family::V4 => "IPv4",
            Family::V6 => "global IPv6",
        };
        bail!(
            "interface \"{name}\" has no {wanted} address; addresses: {}",
            addresses.join(", ")
        );
    };
    Ok(match (ip, port) {
        (IpAddr::V6(v6), Some(port)) => format!("[{v6}]:{port}"),
        (ip, Some(port)) => format!("{ip}:{port}"),
        (ip, None) => ip.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_passes_through() {
        assert_eq!(
            parse_bind_pattern("127.0.0.1:8080").unwrap(),
            BindPattern::Literal("127.0.0.1:8080".into())
        );
        assert_eq!(resolve_bind_addr("0.0.0.0:442").unwrap(), "0.0.0.0:442");
    }

    #[test]
    fn bare_interface_pattern_defaults_to_v4() {
        assert_eq!(
            parse_bind_pattern("%eth0%:442").unwrap(),
            BindPattern::Interface {
                name: "eth0".into(),
                family: Family::V4,
                port: Some(442),
            }
        );
    }

    #[test]
    fn explicit_v4_alias() {
        assert_eq!(
            parse_bind_pattern("%eth0/v4%:442").unwrap(),
            BindPattern::Interface {
                name: "eth0".into(),
                family: Family::V4,
                port: Some(442),
            }
        );
    }

    #[test]
    fn v6_suffix_selects_ipv6() {
        assert_eq!(
            parse_bind_pattern("%eth0/v6%:442").unwrap(),
            BindPattern::Interface {
                name: "eth0".into(),
                family: Family::V6,
                port: Some(442),
            }
        );
    }

    #[test]
    fn portless_pattern_for_admin_bind_address() {
        assert_eq!(
            parse_bind_pattern("%eth0%").unwrap(),
            BindPattern::Interface {
                name: "eth0".into(),
                family: Family::V4,
                port: None,
            }
        );
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        for bad in [
            "%eth0:442",     // missing closing %
            "%%:442",        // empty interface name
            "%eth0/v5%:442", // unknown family suffix
            "%eth0%442",     // junk after closing % without colon
            "%eth0%:hello",  // non-numeric port
        ] {
            assert!(parse_bind_pattern(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn resolves_loopback_interface() {
        assert_eq!(resolve_bind_addr("%lo%:0").unwrap(), "127.0.0.1:0");
        assert_eq!(resolve_bind_addr("%lo%").unwrap(), "127.0.0.1");
    }

    #[test]
    fn unknown_interface_lists_available_ones() {
        let error = resolve_bind_addr("%no_such_iface_xyz%:1").unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("no_such_iface_xyz"), "got: {message}");
        assert!(message.contains("available interfaces"), "got: {message}");
        assert!(message.contains("lo"), "got: {message}");
    }
}
