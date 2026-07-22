# TLS Proxy ACME / RocksDB Rewrite

Last updated: 2026-07-22

All implementation groups requested in this rewrite are complete. This file
is now a resumable completion record rather than an open task list.

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
- `[done]` Opt-in ignored Let's Encrypt and GTS staging account tests exist;
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
