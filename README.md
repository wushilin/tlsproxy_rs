# TLS Proxy

TLS Proxy is a hostname-routed TLS, HTTP, and layer-4 proxy with a RocksDB
control plane and automatic ACME TLS-ALPN-01 certificate management.

## Build

```bash
cargo build --release
```

Rust 1.88 or newer is required by the locked dependency set. A reproducible
multi-stage [Dockerfile](Dockerfile) installs Clang/libclang only in the build
stage. Deployment examples live under `deploy/`.

RocksDB's native build currently requires Clang/libclang development files.
On Debian or Ubuntu, install `libclang-dev`. The management pages are embedded
in the binary and require no Node.js or runtime Internet access.

## First start

```bash
tlsproxy run --runtime-dir /var/lib/tlsproxy
```

An uninitialized runtime starts a temporary HTTPS setup service on a random
loopback port in `40000..=50000`. The log prints its URL, one-time 256-bit
token, and ephemeral certificate SHA-256 fingerprint. To select the setup
address or port explicitly:

```bash
tlsproxy run \
  --runtime-dir /var/lib/tlsproxy \
  --setup-bind 0.0.0.0 \
  --port 44448
```

`--port` is an alias for `--setup-port`; it is not the proxy listener port.
After setup it, setup-token, and setup-bind options are ignored. Operational
configuration comes from RocksDB. The mandatory public TLS listener always
uses port 443 and cannot be deleted or stopped.

Setup asks for:

- the first administrator and a password of at least 12 characters;
- a dedicated control hostname such as `tls.example.com`;
- public IPv4/IPv6 addresses expected in public DNS; and
- an initial ACME provider.

The control hostname is reserved for the form-login administration service and
is never proxied. Its managed certificate is created automatically and the
startup renewal scan attempts issuance immediately. Until issuance succeeds,
the default `local_ca` fallback presents the internal CA certificate.

## Runtime behavior

Every TLS listener supports ordered per-host routes. A route can:

- pass TLS through unchanged;
- terminate TLS and forward plaintext;
- terminate TLS and establish TLS to the upstream; or
- reject the connection.

The mandatory listener additionally intercepts only exact `acme-tls/1`
connections with an active exact-SNI challenge. Unmatched ACME ALPN is closed
and never reaches an upstream. Plain HTTP listeners route by `Host`; raw
forward listeners do not have hostname routing.

DNS overrides are applied after route selection to both inferred and explicit
targets. ACME prerequisites deliberately bypass those overrides and query A
and AAAA records through the configured public resolvers.

## Automatic certificates

The Auto Certs page manages providers and exact-domain or multi-SAN
certificates. Built-in presets are provided for:

- Let's Encrypt production and staging;
- Google Trust Services production and staging.

GTS requires an EAB key ID and base64url HMAC. The HMAC is never returned by
the admin API and is erased from provider metadata after account binding.
Wildcard certificates are rejected because TLS-ALPN-01 cannot validate them.

The scheduler:

- performs one immediate startup scan;
- scans on a fixed 12-hour cadence by default;
- never overlaps scans or certificate operations;
- renews sequentially, normally 15 days before expiry;
- gives each certificate operation a five-minute deadline; and
- atomically activates a validated generation while retaining the previous
  active certificate when renewal fails.

Failed operations use exponential backoff with 20% jitter, capped at 12 hours.
When a CA advertises RFC 9773 ACME Renewal Information, the suggested window is
sampled and persisted; the configured 15-day threshold remains the fallback.
Downloaded chains are checked for issuer ordering and signatures. Per-resolver
DNS results and timestamps are available in the control plane.

Active generations are parsed into an atomically replaced exact-SNI cache.
TLS termination and the control hostname prefer this cache, then apply the
configured `local_ca` or `reject` fallback.

## Administration security

The control hostname serves HTTPS directly on the mandatory listener without
a loopback proxy hop. Authentication uses Argon2id password hashes, opaque
random sessions stored by token hash, `Secure`/`HttpOnly`/`SameSite=Strict`
cookies, CSRF tokens for mutations, and login throttling. Provider secret
fields are redacted. Configuration writes use revisions and cause an orderly
listener reload; a failed apply is automatically rolled back.

The UI also shows runtime/certificate status, DNS diagnostics, recent audits,
configuration history, and rollback controls.

## Certificate publication

Published managed certificates are retrieved by exact domain:

```text
GET /tlsproxy_api/certs/www.example.com
Authorization: Bearer <retrieval-token>
```

The JSON response contains stable `certificate_id` and changing
`generation_id` values, leaf and chain PEM, fingerprint, and validity. Use
`If-None-Match` with the returned ETag for efficient polling. Add
`?private_key=true` only when both the certificate policy and retrieval token
permit key export. Retrieval tokens are independently scoped and expiring;
only their SHA-256 hashes are stored. Requests are rate-limited and audited,
and local-CA fallback identities can never be published.

## Operations

```bash
tlsproxy backup --runtime-dir /var/lib/tlsproxy --output /backup/tlsproxy-2026-07-22
tlsproxy restore --checkpoint /backup/tlsproxy-2026-07-22 --runtime-dir /var/lib/tlsproxy-restored
tlsproxy cleanup --runtime-dir /var/lib/tlsproxy --generations 3 --audit-days 90
tlsproxy recover-admin --runtime-dir /var/lib/tlsproxy --username admin --password-file /run/secrets/new-password
```

Restore refuses a non-empty destination. Administrator recovery is intended
for offline use, revokes existing sessions, and writes an audit record.
Runtime maintenance retains three generations per certificate, 90 days of
audit, fifty configuration revisions, and removes expired sessions daily.

The fully local Pebble interoperability harness is documented in
`tests/pebble/README.md`. It exercises actual TLS-ALPN callbacks to the
mandatory port 443, activation, cache reload, and subsequent serving.

## Legacy YAML migration

YAML is accepted only by the explicit migration command. The destination
database must be uninitialized.

```bash
tlsproxy migrate \
  --config config.yaml \
  --runtime-dir /var/lib/tlsproxy \
  --admin-username admin \
  --admin-password-file /run/secrets/tlsproxy-admin-password \
  --control-hostname tls.example.com \
  --self-ip 203.0.113.10 \
  --provider-id letsencrypt-production
```

Legacy listeners become additional listeners. Migration never guesses which
old listener should become the protected mandatory listener. `genconfig` and
normal YAML `validate`/`run -c` modes no longer exist.

## Logging and shutdown

Logs go to stdout; `RUST_LOG` overrides the default `info` filter. ACME scans,
DNS prerequisites, order lifecycle, listener routing, and connection failures
are logged without account keys, EAB HMACs, certificate private keys, setup
token hashes, or session tokens. Send Ctrl-C for orderly task cancellation and
accounting shutdown.
