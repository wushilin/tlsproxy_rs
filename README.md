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
uses port 443 and cannot be deleted or stopped; its bind address may be
changed, but never its port.

Setup asks for:

- the first administrator and a password of at least 12 characters;
- a dedicated control hostname such as `tls.example.com`;
- public DNS resolvers used for ACME prerequisite checks (required,
  defaulting to `1.1.1.1` and `8.8.8.8`);
- public IPv4/IPv6 addresses expected in public DNS (optional, but required
  before certificates can issue); and
- an initial ACME provider.

The control hostname is reserved for the form-login administration service and
is never proxied. Its managed certificate is created automatically and the
startup renewal scan attempts issuance immediately. Until issuance succeeds,
the default `local_ca` fallback presents the internal CA certificate.

## Runtime behavior

Every TLS listener supports ordered per-host routes. A route can:

- pass TLS through unchanged;
- terminate TLS and forward plaintext;
- terminate TLS and establish TLS to the upstream;
- terminate TLS and reverse-proxy HTTP/1.1 at layer 7; or
- reject the connection.

The layer-7 reverse proxy selects per-request path routes by longest
matching prefix, optionally stripping the prefix, and each path can proxy to
a backend pool, serve static files (with configurable index and directory
listing), or require HTTP Basic Auth. Proxied requests get `X-Forwarded-*`
headers and an optional `Host` override. Plain HTTP listeners route by
`Host` with the same path routing, or redirect to HTTPS (301/308, optional
fixed hostname and port); a dedicated redirect listener protocol also
exists. Raw forward listeners do not have hostname routing.

Backend pools support round-robin and client-IP-hash load balancing. A
global health checker probes every backend endpoint every five seconds over
TCP — plus an HTTP `GET /` for HTTP backends — and routing prefers online
endpoints; results are visible in the control plane.

The mandatory listener intercepts exact `acme-tls/1` connections with an
active exact-SNI challenge. Without an active local challenge, ACME ALPN
follows the ordinary SNI route, so a passthrough backend receives the original
ClientHello unchanged; connections without an allowed route are rejected.

DNS overrides are applied after route selection to both inferred and explicit
targets. ACME prerequisites deliberately bypass those overrides and query A
and AAAA records through the configured public resolvers.

## Automatic certificates

Automatic certificate management can be turned off entirely with the
"Automatic certificates (ACME)" switch on the settings page (`acme.enabled`
in the configuration). While disabled, no automatic certificate records are
created, no ACME orders or renewals are placed, and every terminating
handshake without an active managed certificate uses the certificate
fallback policy — by default the local CA. Already-issued generations keep
serving until they expire.

The Auto Certs page manages providers and exact single-domain certificates
(one domain per certificate; multi-SAN is not supported). Built-in presets
are provided for Let's Encrypt production and staging. Additional RFC
8555-compatible providers can be configured manually.
When a provider requires External Account Binding, its HMAC is never returned
by the admin API and is erased from provider metadata after account binding.
Wildcard certificates are rejected because TLS-ALPN-01 cannot validate them,
and hostnames without a dot are rejected at save time because public CAs
never issue for a single label.

Certificates also register themselves: when a terminating route accepts a
concrete SNI (exact, suffix, or regex match), the domain is registered
automatically after the public-DNS prerequisite confirms it resolves to the
configured self IPs. Registration runs in the background, is throttled to
one attempt per domain per ten minutes, and never delays TLS handshakes.
Suffix matchers accept the bare domain and hosts exactly one label deeper
(`code.rusts3api.example` for suffix `rusts3api.example`, but not
`a.b.rusts3api.example`), so clients cannot spam issuance attempts by
varying deep subdomains; use a regex route if you need deeper trees.

An optional DNS check script (`acme.dns_check_script`, configurable at the
bottom of the Auto Certs page) adds an operator-defined gate on *new*
issuance: when set, the script runs with the candidate domain as its only
argument before any new automatic certificate is created — ACME
registrations and fresh local-CA fallback mints alike. Exit 0 accepts the
domain; any other exit rejects the issuance, and a terminating handshake
that needed the new certificate fails instead of minting one. Spawn
failures fail closed, and a script still running after 30 seconds is
force-killed and treated as a rejection. Certificates that already exist —
active managed certificates and still-cached local-CA leaves — serve
without consulting the script, as does the reserved control-plane
hostname. Domains listed verbatim in an `exact` route entry also skip the
script: an exact route only ever matches the hostname the operator typed,
so there is no room for a client to game it — the script guards the
suffix and regex routes, whose accepted hostnames are client-chosen.
Rejections are remembered for `acme.cache_failure_time_secs` (default 60,
configurable next to the script path) so repeated handshakes for a
rejected hostname do not spawn a process each; acceptances are not cached
at all, because an accepted domain immediately gains a certificate and is
never re-checked. At most four scripts run concurrently. This suits
hostname-style object storage: the script can ask the S3 API whether the
bucket exists before a certificate is ever issued, and a freshly created
bucket starts serving within the rejection-cache window.

A ready-made check script lives in [`dns_script/`](dns_script/): `dnsauth`,
a dependency-light Go program driven by a `config.toml` in its working
directory. It allows a domain if it matches an exact host list, a wildcard
pattern list (each `*` matches exactly one DNS label), or an S3 bucket
pattern such as `*.rusts3api.example.com` — for bucket patterns it sends a
SigV4-signed `HEAD` request to the configured S3 endpoint and allows the
domain only when the bucket exists. Everything else (probe errors, missing
configuration, 403s from bad credentials) fails closed. Build it with
`go build` in that directory and point `acme.dns_check_script` at the
binary; see the commented [`dns_script/config.toml`](dns_script/config.toml)
for all options, including path-style versus virtual-hosted probes and the
recursion guard for endpoints served through this proxy itself.

The scheduler:

- performs one immediate startup scan and scans on a fixed 12-hour cadence
  by default (configurable via `scan_interval_hours`); saving a certificate
  triggers a scan immediately;
- never overlaps scans, but renews up to four certificates concurrently
  within a scan so one slow certificate cannot block the others;
- renews normally 15 days before expiry with a five-minute deadline per
  certificate operation;
- bounds every CA interaction: ARI refresh steps are limited to 30 seconds
  each and scan preparation to ten minutes overall, so a dead CA connection
  can never silently wedge the renewal loop; each scan logs its trigger
  before any network work; and
- atomically activates a validated generation while retaining the previous
  active certificate when renewal fails.

Failed operations use exponential backoff with 20% jitter, starting at five
minutes and capped at 12 hours; a CA-provided Retry-After is honored but
clamped to 48 hours. When a CA advertises RFC 9773 ACME Renewal Information,
the suggested window is sampled and persisted; the configured 15-day
threshold remains the fallback. Downloaded chains are checked for issuer
ordering and signatures. Per-resolver DNS results and timestamps are
available in the control plane.

Every managed certificate can be deleted — pending or issued, automatic or
manual. Accumulated unwanted registrations (for example spam-driven
automatic entries from before the suffix cap and DNS check script existed)
would otherwise consume renewal quota forever. Deleting a certificate whose
TLS route is still active is not permanent: the next matching handshake
re-registers the domain, subject to the DNS check script, and a re-issuance
then counts against CA rate limits — so prune routes or configure the check
script before mass-deleting still-routed domains.

Active generations are parsed into an atomically replaced exact-SNI cache.
TLS termination and the control hostname prefer this cache, then apply the
configured `local_ca` or `reject` fallback.

## Administration security

The control hostname serves HTTPS directly on the mandatory listener without
a loopback proxy hop. Authentication uses Argon2id password hashes, opaque
random sessions stored by token hash, `Secure`/`HttpOnly`/`SameSite=Strict`
cookies, CSRF tokens for mutations, and login throttling. Sessions last 12
hours by default; the login form offers a 30-day remember-me. Provider
secret fields are redacted.

Configuration writes use revisions. Changes limited to listener-internal
settings are hot-applied without rebuilding sockets; anything touching
bind addresses, protocols, or global settings causes an orderly listener
reload, and a failed apply is automatically rolled back to the last known
good revision (which also absorbs successful hot applies).

The UI also shows runtime/certificate status, DNS diagnostics, recent audits,
configuration history, and rollback controls. Administrators can export the
database as JSON and import it back with merge or clear-and-import modes,
optionally scoped to selected column families; imports are audited.

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
lookups are authenticated by SHA-256 token hash, and the bearer value is
deliberately kept recoverable so an administrator can copy an existing token
again. Requests are rate-limited and audited, and local-CA fallback
identities can never be published.

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
