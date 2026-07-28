# Interface-Name Bind Address Syntax Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow listener `bind` and admin `bind_address` to name a network interface (`%eth0%:442`, `%eth0/v4%:442`, `%eth0/v6%:442`) whose IP is resolved at bind time.

**Architecture:** A new pure module `src/bindaddr.rs` parses the pattern (`parse_bind_pattern`) and resolves it against the OS interface table (`resolve_bind_addr`, via the `if-addrs` crate). Config keeps `bind` as a plain `String`; `Config::load_string` validates pattern syntax only. Resolution happens at every bind attempt in `runner.rs`, in the self-connect guard, and at admin server startup.

**Tech Stack:** Rust 2021 (MSRV 1.85), tokio, anyhow, `if-addrs = "0.15"` (new dep).

**Spec:** `docs/superpowers/specs/2026-07-15-interface-bind-syntax-design.md`

## Global Constraints

- No behavior change for existing configs: any value not starting with `%` passes through unchanged.
- Interface *existence* is never checked at config-load time, only syntax.
- Resolution failure fails the listener startup like a bad IP today; no fallback to `0.0.0.0`.
- Error messages must name the interface and list available interfaces / addresses (exact copy in Task 1).
- Run tests with `cargo test`; build with `cargo build`.

---

### Task 1: `bindaddr` module (parser + resolver)

**Files:**
- Modify: `Cargo.toml` (add dependency)
- Create: `src/bindaddr.rs`
- Modify: `src/main.rs` (add `pub mod bindaddr;` to the module list at lines 5-22, alphabetical: after `admin_server`, before `ca`)
- Test: inline `#[cfg(test)] mod tests` in `src/bindaddr.rs`

**Interfaces:**
- Produces: `bindaddr::parse_bind_pattern(input: &str) -> anyhow::Result<BindPattern>`; `bindaddr::resolve_bind_addr(input: &str) -> anyhow::Result<String>`; `enum BindPattern { Literal(String), Interface { name: String, family: Family, port: Option<u16> } }`; `enum Family { V4, V6 }`. Tasks 2 and 3 call these.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` `[dependencies]`, after `zstd = "0.13"`:

```toml
if-addrs = "0.15"
```

- [ ] **Step 2: Write the failing tests**

Create `src/bindaddr.rs` with only the test module for now (types referenced don't exist yet, so add tests + stubs in one file but make tests first mentally; in Rust the file must compile, so write tests and implementation skeleton together — the TDD checkpoint is Step 3's red run using `todo!()` bodies):

```rust
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
    todo!()
}

/// Parses, then resolves an interface pattern to a bindable address string
/// (`ip:port`, `[v6]:port`, or bare IP when the pattern has no port).
/// Literals are returned unchanged.
pub fn resolve_bind_addr(input: &str) -> Result<String> {
    todo!()
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
```

Register the module in `src/main.rs` — after `pub mod admin_server;` add:

```rust
pub mod bindaddr;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test bindaddr`
Expected: FAIL — tests panic with `not yet implemented` (`todo!()`).

- [ ] **Step 4: Implement**

Replace the two `todo!()` bodies:

```rust
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
        let port = port
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("invalid bind pattern `{input}`: invalid port `{port}`"))?;
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
    let ip = matching.iter().map(|interface| interface.ip()).find(|ip| match (family, ip) {
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test bindaddr`
Expected: 8 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/bindaddr.rs src/main.rs
git commit -m "feat: add bindaddr module for %ifname% bind patterns"
```

---

### Task 2: Config-load syntax validation

**Files:**
- Modify: `src/config.rs` (`Config::load_string`, around line 280)
- Test: existing `#[cfg(test)] mod tests` in `src/config.rs`

**Interfaces:**
- Consumes: `crate::bindaddr::parse_bind_pattern(&str) -> anyhow::Result<BindPattern>` from Task 1.
- Produces: `Config::load_string` (unchanged signature `fn(&str) -> Result<Config, Box<dyn Error>>`) now rejects malformed `%...%` patterns in every listener `bind` and in `admin_server.bind_address`.

- [ ] **Step 1: Write the failing tests**

Add to the tests module at the bottom of `src/config.rs` (same style as existing `Config::load_string` tests around line 380):

```rust
#[test]
fn interface_bind_pattern_loads() {
    let yaml = r#"
listeners:
  web:
    bind: "%eth0%:1443"
    target_port: 443
    policy: ALLOW
    rules:
      static_hosts:
        - localhost
      patterns: []
    max_idle_time_ms: 1000
    speed_limit: 0
"#;
    let config = Config::load_string(yaml).unwrap();
    assert_eq!(config.listeners["web"].bind, "%eth0%:1443");
}

#[test]
fn malformed_interface_bind_pattern_fails_load() {
    let yaml = r#"
listeners:
  web:
    bind: "%eth0:1443"
    target_port: 443
    policy: ALLOW
    rules:
      static_hosts:
        - localhost
      patterns: []
    max_idle_time_ms: 1000
    speed_limit: 0
"#;
    let error = Config::load_string(yaml).unwrap_err();
    let message = format!("{error}");
    assert!(message.contains("web"), "got: {message}");
    assert!(message.contains("missing closing"), "got: {message}");
}

#[test]
fn malformed_admin_bind_address_fails_load() {
    let yaml = r#"
listeners: {}
admin_server:
  bind_address: "%eth0/v5%"
"#;
    let error = Config::load_string(yaml).unwrap_err();
    let message = format!("{error}");
    assert!(message.contains("admin_server"), "got: {message}");
    assert!(message.contains("unknown family suffix"), "got: {message}");
}
```

Note: existing config tests show listeners need `target`/`target_port`/`policy`/`rules` etc. — check a nearby passing test (line ~387) and mirror its required fields exactly if deserialization fails on the fixtures above.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests::interface_bind -- --nocapture` and `cargo test malformed_`
Expected: `interface_bind_pattern_loads` PASSES already (no validation needed for good patterns); the two `malformed_*` tests FAIL (load unexpectedly succeeds).

- [ ] **Step 3: Implement validation**

In `Config::load_string` (src/config.rs, around line 280), after the accounting validation:

```rust
pub fn load_string(content: &str) -> Result<Config, Box<dyn Error>> {
    let config: Config = serde_yaml_ng::from_str(content)?;
    if let Some(accounting) = &config.accounting {
        accounting.validate()?;
    }
    for (name, listener) in &config.listeners {
        if let Err(cause) = crate::bindaddr::parse_bind_pattern(&listener.bind) {
            return Err(format!("listener `{name}`: {cause}").into());
        }
    }
    if let Some(admin) = &config.admin_server {
        if let Some(address) = &admin.bind_address {
            if let Err(cause) = crate::bindaddr::parse_bind_pattern(address) {
                return Err(format!("admin_server.bind_address: {cause}").into());
            }
        }
    }
    return Ok(config);
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config`
Expected: all config tests PASS, including the three new ones.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: validate %ifname% bind pattern syntax at config load"
```

---

### Task 3: Runtime resolution wiring (listener bind, self-connect guard, admin server)

**Files:**
- Modify: `src/runner.rs` (bind loop ~line 205, self-connect guard ~line 630)
- Modify: `src/admin_server.rs` (`run()`, ~line 592)
- Test: existing `#[cfg(test)] mod tests` in `src/runner.rs`

**Interfaces:**
- Consumes: `crate::bindaddr::resolve_bind_addr(&str) -> anyhow::Result<String>` from Task 1.
- Produces: private helper `Runner::bind_listener(bind: &str) -> std::io::Result<TcpListener>` (async), used by the bind retry loop and the new test.

- [ ] **Step 1: Write the failing test**

In the tests module of `src/runner.rs` (after `test_client`, ~line 909):

```rust
#[tokio::test]
async fn bind_listener_resolves_interface_pattern() {
    let listener = Runner::bind_listener("%lo%:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.ip().is_loopback());

    let error = Runner::bind_listener("%no_such_iface_xyz%:0")
        .await
        .unwrap_err();
    assert!(format!("{error}").contains("no_such_iface_xyz"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib runner::tests::bind_listener_resolves_interface_pattern`
Expected: FAIL to compile — `bind_listener` not found.

- [ ] **Step 3: Implement**

a) Add the helper to `impl Runner` in `src/runner.rs` (near `start_managed`), plus the import `use crate::bindaddr;` at the top of the file:

```rust
async fn bind_listener(bind: &str) -> std::io::Result<TcpListener> {
    match bindaddr::resolve_bind_addr(bind) {
        Ok(address) => TcpListener::bind(address).await,
        Err(cause) => Err(std::io::Error::other(cause.to_string())),
    }
}
```

b) In `start_managed` (~line 205 and ~line 216), replace both occurrences of:

```rust
let mut listener = TcpListener::bind(&bind).await;
```
and
```rust
listener = TcpListener::bind(&bind).await;
```

with:

```rust
let mut listener = Self::bind_listener(&bind).await;
```
and
```rust
listener = Self::bind_listener(&bind).await;
```

(Resolution now happens fresh on every retry, per spec.)

c) In `reject_obvious_self_connect` (~line 630), replace:

```rust
let Ok(bind_addr) = listener_config.bind.parse::<SocketAddr>() else {
    return Ok(());
};
```

with:

```rust
let Ok(resolved_bind) = bindaddr::resolve_bind_addr(&listener_config.bind) else {
    return Ok(());
};
let Ok(bind_addr) = resolved_bind.parse::<SocketAddr>() else {
    return Ok(());
};
```

d) In `src/admin_server.rs` `run()` (~line 592), replace:

```rust
let bind_address = choose(&config.bind_address, "0.0.0.0".into());
```

with:

```rust
let bind_address = choose(&config.bind_address, "0.0.0.0".into());
let bind_address = crate::bindaddr::resolve_bind_addr(&bind_address)?;
```

(`anyhow::Error` converts into `Box<dyn Error>` via `?`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: full suite PASSES including `bind_listener_resolves_interface_pattern`.

- [ ] **Step 5: Commit**

```bash
git add src/runner.rs src/admin_server.rs
git commit -m "feat: resolve %ifname% bind patterns at bind time"
```

---

### Task 4: Documentation

**Files:**
- Modify: `README.md` (listener config section)

**Interfaces:**
- Consumes: nothing; documents the syntax from Task 1.

- [ ] **Step 1: Document the syntax**

Find the listener `bind` documentation in `README.md` (search for `bind:`) and add after it:

```markdown
The `bind` address may name a network interface instead of a literal IP —
useful on cloud instances where the private IP changes between launches:

```yaml
listeners:
  web:
    bind: "%eth0%:442"      # first IPv4 address of eth0 (same as %eth0/v4%)
    # bind: "%eth0/v6%:442" # first global (non-link-local) IPv6 address
```

The interface's IP is resolved when the listener binds, not when the config
loads. If the interface is missing or has no address of the requested
family, the listener fails to start with an error listing the available
interfaces. `admin_server.bind_address` accepts the same `%eth0%` /
`%eth0/v6%` patterns (without a port).
```

- [ ] **Step 2: Verify build is clean**

Run: `cargo build && cargo test`
Expected: build succeeds, all tests PASS.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document %ifname% bind address syntax"
```
