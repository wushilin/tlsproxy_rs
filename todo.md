# TODO: simplify CA to local-only + on-demand SNI certs + upstream TLS

Status: **implemented in code.** Unit tests, clippy, and build are green; a few
manual end-to-end verification items remain.

## Goal

Remove the MiniCA integration entirely and use only a local CA. Terminating
listeners mint a leaf certificate on the fly for the exact SNI the client
requests (no pre-declared SANs). Add optional TLS to the upstream leg of
termination mode. Keep it as simple as possible.

## Agreed decisions

### CA
- **Drop MiniCA completely** — delete `src/minica.rs`, the `reqwest`
  dependency, `MinicaConfig`, and `CaConfig.minica`. Local CA is the only source.
- New config shape (all paths relative to the runtime folder):
  ```yaml
  ca:
    localca:
      ca_cert: local_ca/CA.pem
      ca_key: local_ca/CA.key
      working_dir: local_ca
  ```
  - `working_dir` (default `local_ca`) is always **under `{runtime_dir}`**.
  - Absent `ca:` / absent fields → defaults above.
- **CA cert/key are persisted** — if the files don't exist they are generated
  once and written; if they exist they are loaded (existing validation rules:
  both-or-neither, is-CA, valid, key matches, self-signature ok).

### Ad-hoc (per-SNI) certificates for termination mode
- On a terminating listener, use rustls **`LazyConfigAcceptor`** to peek the
  ClientHello and read the SNI **before** choosing the ServerConfig.
- **Check the ACL first**, using the SNI — reject denied/garbage SNIs *before*
  minting anything (avoids a cheap keypair-generation DoS).
- Mint a leaf for that single hostname, signed by the local CA.
- **Do NOT persist ad-hoc certs** — cheap to regenerate.
- Cache them in an **LRU capped at 10,000 domains**. Beyond the cap, the
  least-recently-used entry is evicted and that domain re-mints on its next
  request.
- **No `san` field on listeners** anymore.
- **No explicit `tls_cert`/`tls_key` on listeners** anymore — remove the fields;
  termination always uses the local-CA on-demand path.

### Renewal (no lazy per-handshake renewal)
- All generated leaf certificates, including the admin cert, are valid for
  **365 days**.
- A periodic **eviction job** removes cached certs that are **within 72 hours of
  expiry**, so they get freshly minted on the next connection.
- Eviction job runs **every 1 hour**.
- (The 72h-before-expiry sweep is a
  safety net for very long-running processes. LRU cap handles the normal churn.)

### Admin server cert
- Still served over the static, hot-reloadable `RustlsConfig` (axum-server).
- Cached in memory like ad-hoc certs, but **persisted** under
  `{runtime_dir}/{working_dir}`, e.g. `admin-cert.pem` / `admin-key.pem`,
  covering `admin_server.san`.
- On restart, load the persisted admin cert if present and valid, then
  immediately check the 72h eviction window; if it is within 72 hours of expiry,
  evict it from memory so the next admin access mints and saves a fresh cert.
- The hourly eviction job also applies to the admin cert. If it is evicted
  within 72 hours of expiry, leave it absent from the in-memory cache; the next
  admin access mints a fresh 365-day admin cert, saves it, and serves it.

### Upstream TLS for termination mode
- New per-listener field `upstream_tls: bool` (default `false` → preserves
  today's plaintext-upstream behavior).
- When `true`: after the TCP connect to the upstream, wrap the stream in a
  rustls **client** connector using the SNI hostname as the server name, then
  relay over that (the `relay`/`pipe` chain is already generic over
  `AsyncRead/Write`).
- **Trust-all**: rustls `ClientConfig` with a no-op certificate verifier
  (`dangerous().set_certificate_verifier(...)`) that accepts any cert. Document
  clearly: upstream leg is **encrypted but NOT authenticated** (no MITM
  detection). No `upstream_ca` pinning for now.

## Work items

- [x] **config.rs**
  - [x] Remove `MinicaConfig`; remove `CaConfig.minica`.
  - [x] `LocalCaConfig { ca_cert, ca_key, working_dir }` with defaults
        (`local_ca/CA.pem`, `local_ca/CA.key`, `local_ca`).
  - [x] `Listener`: remove `san`, `tls_cert`, `tls_key`; add
        `upstream_tls: bool` (`#[serde(default)]`).
  - [x] Fix `Default`/test constructors accordingly.
- [x] **ca.rs**
  - [x] Drop `CaSource::Minica`; local-only (can collapse the enum).
  - [x] Resolve `working_dir`/ca paths under `{runtime_dir}`.
  - [x] LRU cert cache (add `lru` crate) keyed by sanitized hostname, cap 10k.
  - [x] `resolve_or_mint(sni) -> CertifiedKey` (mint via local CA, cache).
  - [x] Hourly eviction job: drop entries within 72h of expiry; for the admin
        cert, leave it absent so the next admin access re-mints and persists it.
  - [x] Remove the old per-identity daily renewal job (minica-era) or repurpose
        for the admin cert only.
- [x] **certificate.rs**
  - [x] Keep local CA load/create + `prepare_local_identity` (admin cert).
  - [x] Add a helper that mints a leaf for one hostname and returns a rustls
        `CertifiedKey` (in-memory, no disk write).
- [x] **runner.rs**
  - [x] Replace `TlsAcceptor::from(fixed_config).accept()` in
        `worker_terminate` with `LazyConfigAcceptor`: peek SNI → ACL check →
        get/mint cert from `ca` → build ServerConfig → complete handshake.
  - [x] Implement `upstream_tls`: optional rustls client (trust-all) around the
        upstream TCP stream before `relay`.
  - [x] Remove the `SharedServerConfig` per-listener plumbing if no longer
        needed (per-SNI resolver replaces it); keep for admin only.
  - [x] Re-check self-connect/loop guard for terminate+upstream_tls (we now send
        a fresh ClientHello upstream, so the ClientHello-random cache won't catch
        a terminate→self loop — add a guard if there's a real hole).
- [x] **admin_server.rs** — obtain admin cert from the local CA cache, loading
      from disk on restart, immediately evicting if inside the 72h window, and
      saving after lazy renewal.
- [x] **manager.rs** — build the local CA source once, pass to runners.
- [x] **main.rs** — remove `pub mod minica`; keep the `aws_lc_rs`
      `install_default()` (still needed for the rustls client + server).
- [x] **Cargo.toml** — remove `reqwest`; add `lru`; confirm `rustls` default
      features still provide `aws-lc-rs` so `install_default()` works.
- [x] **static/app.js + index.html** — remove listener `san` / `tls_cert` /
      `tls_key` fields; add an `upstream_tls` toggle in terminate mode; update
      validation (terminate no longer needs cert paths or SANs).
- [x] **config.yaml + README** — new `ca.localca` block; document `upstream_tls`
      and the trust-all caveat; drop the listener SAN/cert-path docs.

## Verification (end-to-end)

- [ ] Default config (no `ca:`): terminate listener mints a per-SNI cert from the
      local CA under `{runtime_dir}/local_ca`, chain-verifies against `CA.pem`;
      no `san` needed.
- [ ] Two different SNIs to the same listener get two different leaf certs, both
      valid; ad-hoc certs are NOT written to disk.
- [ ] Denied SNI is rejected before any cert is minted.
- [ ] `upstream_tls: true` — proxy connects to a TLS upstream, ignores its cert,
      relays both directions.
- [x] Admin server still serves HTTPS from the local CA (cached in memory,
      loaded on restart, immediately evicted if near expiry, persisted after
      lazy renewal). Covered by admin cert cache/persistence tests.
- [x] `cargo test` green; existing tests updated for the new Listener fields.

## Confirmed decisions
1. Admin cert is cached in memory, loaded from disk on restart, immediately
   evicted if within 72 hours of expiry, and saved after lazy renewal.
2. Global eviction threshold is 72 hours before expiry for ad-hoc and admin
   certs.
3. All generated leaf certs, including the admin cert, are valid for 365 days.
4. `working_dir` is always forced under `{runtime_dir}`.
