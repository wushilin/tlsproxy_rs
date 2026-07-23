# TLS Proxy ACME / RocksDB Rewrite

Last updated: 2026-07-22

All implementation groups requested in this rewrite are complete. This file
is now a resumable completion record rather than an open task list.

## Deferred certificate-policy UX

- `[todo]` When a configuration adds a hostname that requires TLSProxy to
  present a certificate, and the hostname has no managed certificate, require
  an explicit certificate policy:
  - **Local CA certificate:** issue and persist a leaf signed by the local CA,
    and renew it automatically before expiry.
  - **Automatic public certificate:** select an existing ACME provider,
    automatically create the managed-certificate record, then issue, activate,
    persist, and renew it. Explicitly choose whether initial issuance failures
    temporarily fall back to the local CA or reject TLS handshakes.
  - **Reject until configured:** keep the route but reject TLS handshakes until
    a valid managed certificate is active.
- `[todo]` At handshake time, continue preferring an active managed certificate
  for the normalized SNI hostname; use the configured hostname policy only when
  no managed certificate is available. Do not silently initiate public ACME
  issuance merely because a terminating route was created.
- `[todo]` Add an explicit scheduled lifecycle for persisted local-CA leaf
  certificates so the UI can accurately promise automatic renewal. A local-CA
  leaf is CA-signed, not self-signed; clients must trust the local root CA.
- `[todo]` Simplify exact-host route identity: use the normalized hostname as
  the user-facing route name and lookup key within a listener. Normalize to
  lowercase, remove a trailing dot, reject duplicate hostnames in the same
  listener, and allow the same hostname on different listeners/ports. Keep the
  default route separate and reject wildcard route entries until wildcard
  routing has a concrete use case. If audit/history requires stable identity,
  retain an internal generated route ID that is not exposed as a second name.
- `[todo]` Add a first-class HTTP reverse-proxy listener, with client-side TLS
  independently optional. For HTTPS, validate supported SNI hostnames, select
  the managed/local certificate, terminate TLS, parse HTTP, and route using the
  normalized HTTP `Host` header. Require SNI and HTTP `Host` compatibility by
  default to prevent unintended domain fronting; make any exception explicit.
- `[todo]` Replace the separate HTTP-forwarding/"HTTP passthrough" design with
  this reverse-proxy listener. Plain HTTP forwarding is the minimal reverse-
  proxy configuration: client TLS disabled, plaintext upstream, one backend,
  and no health checking, load balancing, or `Host` rewrite. Migrate existing
  persisted HTTP listener configuration through the revisioned configuration
  path rather than maintaining two overlapping models. Keep raw TCP forwarding
  and encrypted TLS passthrough as distinct Layer-4 features.
- `[todo]` Configure reverse-proxy behavior per exact host. Each host route may
  contain multiple backend targets; each backend independently selects HTTP or
  HTTPS transport. Keep upstream TLS SNI separate from the optional HTTP `Host`
  header rewrite because they may need different values. Preserve forwarding
  metadata (`Forwarded`/`X-Forwarded-*`) and support WebSocket upgrades; decide
  HTTP/2 behavior explicitly.
- `[todo]` Add active health checks for reverse-proxy backend pools, including
  configurable path, interval, and timeout. Exclude unhealthy backends from
  selection and return `503 Service Unavailable` when none are healthy.
- `[todo]` Support per-host load-balancing policies: round robin and sticky
  client-IP consistent hashing. Hash stable backend IDs over the healthy set to
  limit remapping when membership changes, and fail over when the selected
  backend becomes unhealthy. Use the direct peer IP unless the immediate peer
  is in an explicitly configured trusted-proxy network; only then accept a
  client address from `Forwarded` or `X-Forwarded-For`. Document IP affinity as
  best effort rather than guaranteed session persistence.

## Runtime, administration, and managed serving

- `[done]` RocksDB-first bootstrap/runtime, mandatory 443 listener, per-host
  TLS and HTTP behavior, raw forwarding, DNS overrides, accounting, safe
  reload, and automatic rollback.
- `[done]` Form login with Argon2id, hash-only sessions, secure cookies, CSRF,
  throttling, password change, provider/certificate/configuration APIs, and
  staged control-hostname changes.
- `[done]` Atomic managed generation activation and exact-SNI cache selection
  for TLS termination and the control hostname.

## Certificate publication

- `[done]` `GET /tlsproxy_api/certs/{domain}` returns stable certificate ID,
  generation ID, leaf/chain PEM, fingerprint, validity, and policy-gated key
  PEM.
- `[done]` Caller-visible ETags and `304 Not Modified` are supported.
- `[done]` Retrieval bearer tokens are separate from admin sessions, scoped,
  expiring, returned once, and stored only by SHA-256 hash.
- `[done]` Publication requires explicit managed-certificate policy, applies
  rate limiting and accepted/rejected audit, and cannot expose fallback certs.

## ACME interoperability and resilience

- `[done]` The Docker Pebble/challtestsrv harness performs an actual RFC 8555
  order and three TLS-ALPN callbacks to mandatory port 443, then verifies
  atomic storage, cache reload, and subsequent TLS serving. It passed locally.
- `[done]` An opt-in ignored Let's Encrypt staging account test exists;
  normal tests never contact public CAs.
- `[done]` Issuer chain ordering/signatures are validated and per-resolver DNS
  outcomes/timestamps are persisted and displayed.
- `[done]` Failed renewals use exponential backoff with ±20% jitter capped at
  12 hours; manual renewal clears retry gates.
- `[done]` RFC 9773 ARI suggestions and recheck hints are consumed and stored;
  unsupported CAs retain the configured 15-day fallback. Replacement orders
  send the prior certificate identifier when supported.

## Operations and cleanup

- `[done]` `backup` creates a consistent RocksDB checkpoint and `restore`
  validates an empty destination and initialized result.
- `[done]` Daily and manual retention bound generations, sessions, audit, and
  configuration history.
- `[done]` `recover-admin` resets/creates an administrator offline, revokes
  sessions, and audits recovery.
- `[done]` Dead manager/admin server modules were deleted. Shared target
  admission and byte relay moved to `relay`; outbound TLS moved to
  `upstream_tls`; normal runtime no longer depends on `Runner`. The isolated
  legacy YAML conversion types remain only because the explicit one-way
  `migrate` command is still a supported compatibility boundary.
- `[done]` UI includes status, listeners/settings, certificate state,
  providers, publishing tokens, DNS diagnostics, audit, revision history, and
  rollback.
- `[done]` Docker, Compose, and hardened systemd examples are included.
  Release builds use Rust 1.88 and install libclang only in the builder.

## Verification

- `[done]` `cargo check` is warning-free.
- `[done]` Offline suite: 125 passed, 2 deliberately ignored public staging
  tests; Pebble harness contract test also passed.
- `[done]` Full isolated Pebble integration passed with real `acme-tls/1`
  callbacks, issuance, ARI query, activation, and managed serving.
- `[done]` Release Docker image built successfully with the locked dependency
  set and corrected Rust 1.88 minimum.
