# Interface-Name Bind Address Syntax

**Date:** 2026-07-15
**Status:** Approved

## Problem

Listener `bind` and admin `bind_address` require a literal IP (`ip:port`). On
cloud instances the private IP changes between instance launches, forcing a
config edit on every new instance. We need a way to say "bind to whatever IP
interface eth0 has right now".

## Syntax

- `%<ifname>%:<port>` — resolves to the interface's first IPv4 address.
  Example: `%eth0%:442`.
- `%<ifname>/v4%:<port>` — explicit alias for the default IPv4 behavior.
- `%<ifname>/v6%:<port>` — resolves to the interface's first *global*
  (non-link-local) IPv6 address, formatted as `[addr]:port`.
- Any value not starting with `%` is passed through unchanged; all existing
  configs behave identically.
- The `%` characters are YAML-safe without quoting.

Applies to both listener `bind` (which includes the port) and admin
`bind_address` (IP only — the same `%eth0%` / `%eth0/v6%` pattern without a
port suffix).

## Approach

A new module `src/bindaddr.rs` exposes:

- `parse_bind_pattern(&str) -> Result<BindPattern>` — pure syntax parsing,
  no OS calls. `BindPattern` is either `Literal(String)` or
  `Interface { name, family, port: Option<u16> }`.
- `resolve_bind_addr(&str) -> Result<String>` — parses, then resolves the
  interface via the [`if-addrs`](https://crates.io/crates/if-addrs) crate
  (thin, maintained, cross-platform wrapper over `getifaddrs`). Returns a
  bindable string (`ip:port`, `[v6]:port`, or bare IP for the admin case).

`Listener.bind` stays a plain `String` in `config.rs`, so serde serialization
round-trips the pattern, never a resolved IP.

### Resolution points (all runtime, never at config load)

1. **Listener bind** — [runner.rs:205](../../src/runner.rs): resolve fresh on
   each of the existing 3 bind retries, so an interface that appears slightly
   late is still caught.
2. **Self-connect guard** — [runner.rs:630](../../src/runner.rs): resolve the
   pattern before parsing to `SocketAddr` so loop detection keeps working.
3. **Admin server** — [admin_server.rs:592](../../src/admin_server.rs):
   resolve `bind_address` before constructing the `SocketAddr`.

### Failure handling

Resolution failure fails the listener startup exactly like a bad IP today,
with a precise error:

- Interface missing: `interface "eth0" not found; available interfaces: lo,
  ens5, docker0`
- No address of the requested family: `interface "eth0" has no global IPv6
  address; addresses: 10.0.0.5, fe80::...`

No fallback to `0.0.0.0`; no indefinite retry beyond the existing 3 attempts.

### Config-load validation

`Config::load_string` validates pattern *syntax* for every listener bind and
the admin bind_address: `%eth0:442` (unclosed), `%eth0/v5%` (unknown family
suffix), `%%:442` (empty name) are load-time errors. Interface *existence* is
deliberately not checked at load, so a config written for another machine
still validates.

## Alternatives rejected

- **Resolve at config load:** bakes the IP in; restarts/reloads would not
  re-resolve, and serialized config would leak the concrete IP.
- **No crate (raw getifaddrs / /proc):** Linux-only or unsafe FFI; not worth
  avoiding one small dependency.

## Testing

- Parser unit tests: pass-through literals, bare v4 pattern, explicit `/v4`
  alias, `/v6` pattern, malformed patterns rejected (unclosed `%`, empty
  name, unknown suffix).
- Resolution test against `lo` (exists on every CI/dev box): `%lo%:0`
  resolves to `127.0.0.1:0`.
- Not-found test: bogus interface name yields error mentioning the name and
  listing available interfaces.
- Config validation test: bad pattern in listener bind fails
  `Config::load_string`; good pattern loads.
