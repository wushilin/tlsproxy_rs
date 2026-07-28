# Planned changes

Living document. Each numbered section is one planned change; items are added as they come up
and checked off when implemented.

---

## 1. Standardize DNS overrides (structured rules with host + port matchers)

Status: **draft — pending review**

### 1.1 Background: current behavior and its problems

The resolver is keyed on the single string `"{sni_host}:{listener_target_port}"` (lowercased).
Rules live in the `dns:` YAML map and their type is inferred from magic key prefixes:

| Key form            | Behavior today                                                        |
|---------------------|-----------------------------------------------------------------------|
| `host:port`         | Exact string match against the combined `host:port` string            |
| `host` (no port)    | **Never matches** — lookup key always contains `:port` (silent trap)  |
| `suffix:<s>`        | `ends_with(s)` on the combined string, longest suffix wins            |
| `regex:<r>`         | Regex over the combined string, insertion order                       |

Problems:

- Magic prefixes are undiscoverable; the UI is a bare key/value table with no hint of the four forms.
- Port is not a first-class concept — it is baked into a string, so "any port" or "port range"
  cannot be expressed, and the bare-host form silently does nothing (the `home.wushilin.net`
  entry in the current config.yaml is dead configuration today).
- Suffix matching over the combined string conflates host and port
  (`suffix:example.com:443` cannot say "example.com on any port").
- No way to preserve the original port when rewriting only the host.

### 1.2 Goals

- Structured rules: explicit `type` (exact | suffix | regex) + separate host matcher and port matcher.
- Port matcher is an **exact port only** (no ranges, no `*` syntax). Leaving the port empty means
  "any port" — needed so suffix/regex rules aren't bound to one port and so legacy bare-host
  entries can migrate.
- Deterministic, documented resolution order (see 1.4), surfaced in the UI.
- Targets accept hostnames or IP addresses; target port is optional — null/omitted means
  "use the incoming port".
- **No legacy compatibility** (decided): a config file with the old string-map `dns:` form
  fails at startup with a copy-pasteable migration error; the admin API accepts only the new
  array shape.

### 1.3 New rule model

Config file (`dns` changes from a map to a list; list order is the tie-breaker of last resort
and the significant order for same-specificity regex rules):

```yaml
dns:
  - type: exact          # exact | suffix | regex
    host: example.com    # exact hostname, suffix string, or regex depending on type
    port: 443            # integer 1-65535, or null/omitted = any port
    target_host: 10.0.0.10
    target_port: 8443    # 1-65535, or null/omitted = use the incoming port
  - type: suffix
    host: .internal.corp # plain string suffix of the hostname (dot-anchor by writing the dot)
    port: null           # any port
    target_host: gateway.corp
    target_port: null    # keep the incoming port
  - type: regex
    host: "^api-\\d+\\."
    port: 8443
    target_host: api-pool.corp
    target_port: 443
```

Admin API (`GET/PUT /apiserver/config/dns`) exchanges the same shape as a JSON array.

Semantics:

- `host` matching is case-insensitive (regexes compiled case-insensitive, as today).
- `type: suffix` matches when the hostname equals the suffix or ends with it. Users who want
  label-boundary matching write a leading dot (`.example.com`); we do not force it.
- `type: regex` matches the **hostname only** (not `host:port` — see behavior change in 1.5).
- `port` set → matches that port exactly; `port` null → matches any port.
- `target_port: null` uses the port of the incoming connection — the natural companion to
  any-port rules.

### 1.4 Resolution order

Primary key: host specificity. Secondary key: port specificity. Final tie-breaker: rule order
in the config.

1. **Host tier:** exact > suffix > regex.
2. Within the suffix tier: longer suffix wins (as today).
3. Within one host tier (and same suffix length): a rule **with a port** beats a rule with
   **any port**.
4. Still tied (e.g. two regex rules that both match): first rule in the list wins.

Worked examples (request `api.internal.corp:8443`):

| Rule                                            | Matches? | Why it wins / loses                         |
|-------------------------------------------------|----------|---------------------------------------------|
| exact `api.internal.corp`, any port             | yes      | wins — exact host beats everything below    |
| suffix `.internal.corp`, port `8443`            | yes      | loses to exact host despite exact port      |
| suffix `.internal.corp`, any port               | yes      | loses to the same suffix with exact port    |
| regex `^api-.*`, port `8443`                    | no       | regex only consulted after suffix tier      |

Implementation: rules are compiled once into a single `Vec<CompiledRule>` pre-sorted by
`(host_tier, -suffix_len, has_port_desc, config_index)`; `resolve()` walks it and returns the
first match. This replaces the current three separate global maps and makes the precedence
testable in one place.

### 1.5 Legacy config handling: fail fast (decided)

There is **no silent auto-migration**. When `dns:` is the legacy map form, startup fails with
an error that does the migration work for the user: each legacy entry is printed together with
its suggested new-format rule, ready to paste into config.yaml. The suggestion generator uses
this conversion table:

| Legacy entry                | Suggested rule                                                       |
|-----------------------------|----------------------------------------------------------------------|
| `host: target`              | exact / host / any port (note: this entry never matched before —    |
|                             | the lookup key always contained `:port`)                             |
| `host:port: target`         | exact / host / port exact                                            |
| `suffix:S:port: target`     | suffix / `S` / port exact (trailing `:digits` split off)             |
| `suffix:S: target`          | suffix / `S` / any port                                              |
| `regex:R: target`           | regex / `R` / any port                                               |
| target `host` (no port)     | `target_host`, `target_port: null` (use incoming port)               |
| target `host:port`          | `target_host` + `target_port`                                        |

Suggestions carry warnings where semantics shift:

1. Bare-host entries never matched under the old resolver; the suggested rule *will* match
   (any port) — the warning says so.
2. Legacy regexes ran against the combined `host:port` string; new regexes match the hostname
   only. A pattern that looks port-anchored (contains `:` or a trailing `\d+$`) is flagged so
   the user restates it as host regex + exact port.

The admin API accepts only the new array shape; a PUT with the legacy map form returns 400.

### 1.6 Backend changes (file by file)

- `src/config.rs` — `dns` field becomes `Vec<DnsRule>`. New `DnsRule { type, host,
  port: Option<u16>, target_host, target_port: Option<u16> }`. A custom deserializer detects
  the legacy map form and returns the migration error described in 1.5 (it parses the legacy
  entries only to generate the suggested rules, never to run with them).
- `src/resolver.rs` — rewrite around `Vec<CompiledRule>` sorted per 1.4;
  signature becomes `resolve(host: &str, port: u16) -> Option<(String, Option<u16>)>`
  (target host + optional fixed port). Keep the info-level match logging including which rule won.
- `src/runner.rs` (`resolve_target`) — pass `sni_target` and `listener_config.target_port`
  separately instead of formatting `"{host}:{port}"`; apply `target_port` fallback to the
  original port when the rule preserves it (replaces today's `parse_or_default` dance).
- `src/admin_server.rs` — `GET/PUT /apiserver/config/dns` serialize `Vec<DnsRule>`; PUT
  validates (see 1.8) and returns 400 with a message on invalid rules.

### 1.7 UI changes (DNS page)

- Replace the key/value table with a **rule table** sorted by effective precedence:
  columns: `#` (effective order), Type badge, Host, Port, →, Target host, Target port
  ("preserve" shown when null), Remove.
- Add-rule form: Type select (exact / suffix / regex) that swaps the host-field label,
  placeholder, and helper text; Port field with helper "leave empty to match any port";
  Target host; Target port with helper "leave empty to keep the incoming port".
- Up/down reorder buttons shown only where order matters (ties — primarily regex rules).
- A visible "How rules are matched" panel restating 1.4 in one sentence:
  *exact host → longest suffix → regex; within a tier, exact port → port range → any port.*
- **Rule tester**: an input (`host:port`) that evaluates the pending rules client-side with the
  same precedence logic and highlights the winning rule — makes the magic inspectable before
  saving.
- Validation errors shown inline on the add form (regex validity, port syntax) in addition to
  save-time validation.

### 1.8 Validation (client and server, same rules)

- `type` ∈ {exact, suffix, regex}; `host` non-empty; regex must compile.
- `port`: null or integer 1-65535.
- `target_host` non-empty, no whitespace; `target_port` null or 1-65535.
- Duplicate detection: same (type, host, port) tuple rejected.

### 1.9 Tests

- Unit tests in `resolver.rs`: each tier of the precedence table in 1.4, including
  suffix-length tie-break, with-port-beats-any-port, config-order tie-break, and
  incoming-port preservation.
- Config tests: legacy map form fails startup with the expected suggestions (every row of the
  1.5 table appears in the error output); new-format config round-trips load → save → load.
- API tests: PUT invalid rule → 400 with message; PUT legacy map form → 400; PUT valid →
  GET round-trips.
- Keep the existing `resolution_priority_and_regex_order_are_deterministic` test, ported to
  the new API.

### 1.10 Decisions (resolved 2026-07-07)

1. **Host specificity strictly dominates** — exact host + any port beats suffix host + exact
   port. Confirmed.
2. **No legacy support anywhere** — legacy map form in config.yaml fails at startup (with
   migration suggestions, see 1.5); the admin API rejects it with 400. Confirmed.

---

## 2. minica integration (managed CAs, auto-issued TLS-termination certs, auto-renewal)

Status: **draft — pending review**

Reference: <https://github.com/wushilin/minica> — REST CA service, Basic auth
(username/password), CAs addressed by **CA ID**, certificate issuance by CN + hostname/IP SANs,
downloads in PEM (cert, key, CA cert), API explorable at `/swagger`. The `mcacli` env contract
(`MINICA_URL`, `MINICA_USER`, `MINICA_PASSWORD`, `MINICA_CA_ID`) confirms the four connection
parameters this feature needs.

### 2.1 Goals

- Configure one or more named minica connections (URL + CA ID + username + password) and
  **test** them from the UI before use.
- TLS-termination listeners can choose "issue my certificate from CA X" instead of pointing at
  manual cert/key files.
- A daily background job inspects every minica-issued certificate; anything expiring soon is
  renewed against minica and the new cert/key written and picked up. The same job can be
  **triggered on demand from the admin UI** (decided).
- Renewal is **hot-swap** for proxy listeners (decided): new cert material swaps in via
  ArcSwap without dropping active connections.
- The **local CA mechanism is removed** (decided): no more `CA.pem`/`CA.key` generation. The
  admin server's own certificate is issued from a configured minica CA, with SANs taken from
  the admin server's configured hostnames/IPs.

### 2.2 Config schema

```yaml
certificate_authorities:        # top-level, named like listeners
  corp-ca:
    url: https://minica.corp:9988/minica
    ca_id: 1f0c2e6a-…           # minica CA ID
    username: admin
    password: secret
    tls_ca_cert: null           # optional PEM to trust for the minica endpoint itself
    insecure_skip_verify: false # for lab minica instances with self-signed certs
    renew_before_days: 30       # per-CA (decided): renew certs expiring within N days
```

Admin server (replaces the local-CA fields):

```yaml
admin_server:
  tls: true
  # explicit files still work:
  tls_cert: null
  tls_key: null
  # or issue from minica (replaces auto_generate_tls + CA.pem/CA.key):
  cert_source: minica           # manual | minica
  ca_name: corp-ca
  certificate_hostnames: [localhost, proxy.corp]   # kept — SANs for the issued cert
  certificate_ip_addresses: [127.0.0.1, ::1]       # kept — SANs for the issued cert
  # REMOVED: auto_generate_tls, certificate_ca_path, certificate_ca_key_path,
  #          generated_cert_path, generated_key_path, certificate_validity_days
```

The issued admin cert's CN is the first entry of `certificate_hostnames`; validity is decided
by the minica CA's policy (passed as an issuance parameter only if the minica API exposes one).
Consistent with the 1.5 fail-fast philosophy, a config still containing the removed local-CA
fields fails at startup with a message naming the replacement (`cert_source: minica` +
`ca_name`), and `cert_source: minica` without a matching `certificate_authorities` entry is a
startup error too.

Listener additions (only meaningful for `mode: terminate`):

```yaml
listeners:
  HTTPS:
    mode: terminate
    cert_source: minica         # manual (default, = today) | minica
    ca_name: corp-ca            # which certificate_authorities entry
    cert_cn: proxy.corp         # CN for the issued cert
    cert_hostnames:             # SANs: DNS names and/or IPs
      - proxy.corp
      - 10.0.0.5
    # tls_cert / tls_key remain and are used when cert_source = manual
```

Issued material is stored in a managed directory, out of the user's way:
`minica-certs/<listener>/cert.pem`, `key.pem`, `ca.pem`. Path derived, not configurable per
listener (keeps the listener config small); base dir overridable via
`options.minica_cert_dir`.

### 2.3 Issuance and renewal flow

- **Issue on demand:** when a `cert_source: minica` listener (or the admin server) starts and
  has no stored cert, or its CN/SANs changed vs. the stored cert, request a new cert from
  minica, write files atomically (`tmp` + rename), then load them.
  Exception: if stored material exists but is stale/expiring, startup does **not** wait on
  minica — see the non-blocking rule below.
- **Renewal job** — one shared code path with three triggers:
  1. **At startup, non-blocking (decided):** listeners and the admin server come up
     immediately on whatever stored material exists (even expiring or expired); the renewal
     check runs in the background and hot-swaps replacements as they arrive. Only a listener
     with *no* usable material at all blocks on first issuance.
  2. **Daily tick** (every 24 h).
  3. **On demand** from the admin UI ("Run renewal check now", decided).
- The job itself:
  1. Enumerate stored certs: all `cert_source: minica` listeners plus the admin server cert.
  2. Parse expiry with `x509-parser` (already a dependency).
  3. If `not_after - now < renew_before_days` (per-CA setting), request a fresh cert from
     minica and replace the files atomically.
  4. **Hot-swap (decided):** proxy listeners serve the new cert without dropping connections —
     the rustls `ServerConfig` lives behind an `ArcSwap`; each accepted connection loads the
     current Arc, so in-flight connections finish on the old cert and new ones use the new.
     Caveat: the admin server runs on rocket, whose TLS stack is fixed at launch — renewing
     the admin cert restarts the rocket server only (proxy traffic unaffected; one admin-UI
     blip). If rocket ever exposes a cert resolver hook this gets folded into the same
     ArcSwap path.
  5. On failure: keep serving the existing material, log a warning, retry at the next tick;
     surface last-attempt status in the UI (2.5).
- Renewals and issuance go through a single `minica_client` module so the exact endpoint
  paths live in one place. **The concrete REST paths/payloads are confirmed against the
  minica `/swagger` spec during implementation** — the plan intentionally doesn't guess them.

### 2.4 Backend changes (file by file)

- `Cargo.toml` — add `reqwest` with `rustls-tls` and `arc-swap` (everything else needed —
  `x509-parser`, `rustls`, `chrono` — is already present).
- `src/minica_client.rs` (new) — typed client: `test_connection()` (auth + CA lookup),
  `issue_cert(cn, hostnames) -> (cert_pem, key_pem, ca_pem)`; Basic auth; optional custom
  trust root / skip-verify per 2.2.
- `src/cert_manager.rs` (new) — owns the managed cert directory, expiry inspection, the
  issue-if-missing logic, the renewal task (startup non-blocking pass, daily tick, on-demand
  trigger), and the per-listener `ArcSwap<rustls::ServerConfig>` handles; exposes status
  (`{subject, cn, not_after, last_attempt, last_error}`) for the admin API.
- `src/certificate.rs` — local CA generation (`CA.pem`/`CA.key`, self-issued admin leaf)
  **removed**; what remains is PEM load/parse helpers shared with `cert_manager`.
- `src/config.rs` — `certificate_authorities` map; listener fields (`cert_source`, `ca_name`,
  `cert_cn`, `cert_hostnames`); admin-server fields per 2.2 with startup errors for removed
  local-CA fields. Validation: `ca_name` must reference an existing CA, `cert_cn` required
  when `cert_source: minica`.
- `src/runner.rs` / `src/manager.rs` — terminate-mode listeners read their rustls config
  through the `cert_manager` ArcSwap handle per accepted connection (manual-file listeners get
  a static config behind the same handle type, so the accept path has one shape).
- `src/admin_server.rs` — new endpoints:
  - `GET/PUT /apiserver/config/cas` — CRUD for CA connections.
  - `POST /apiserver/cas/<name>/test` — live connection test, returns ok / error message.
  - `GET /apiserver/certs/status` — cert status (listeners + admin cert) for the UI.
  - `POST /apiserver/certs/<subject>/renew` — force renew one cert.
  - `POST /apiserver/certs/renew-all` — run the renewal check now (the on-demand trigger).

### 2.5 UI changes

- New sidebar page **"Certificates"** (between DNS overrides and Admin security):
  - **CA connections panel** — table of configured CAs (name, URL, CA ID, masked password,
    renew_before_days), add/edit/remove, and a **Test** button per row showing the live
    result inline.
  - **Issued certificates panel** — one row per minica-issued cert (each terminate listener
    plus the admin server): subject, CN, SANs, expiry (warning badge inside the renew window,
    danger when expired), last renewal attempt/result, and a **Renew now** button per row.
  - A **"Run renewal check now"** button running the whole job on demand (decided), with the
    results reflected in the table.
- Listener card (Listeners page), terminate mode: a "Certificate source" select —
  **Manual files** (shows today's cert/key path fields) or **Issue from CA** (shows CA
  dropdown populated from configured CAs, CN field, SANs textarea one-per-line).
- **Admin security page**: the "Managed local CA" panel and the CA.pem install note are
  removed. In their place: certificate source (Explicit files / Issue from minica CA +
  CA dropdown), the SAN hostname/IP fields (kept), and current admin-cert expiry with a note
  that renewing the admin cert briefly restarts the admin server only.
- Validation: choosing "Issue from CA" anywhere with no CAs configured links the user to the
  Certificates page.

### 2.6 Security notes

- CA passwords land in `config.yaml` plaintext — consistent with the existing
  `admin_server.password`, so no new storage mechanism; noted for a possible future
  secrets-file change (out of scope here).
- API `GET /apiserver/config/cas` returns passwords masked (`***`); `PUT` treats the mask
  sentinel as "keep the stored password" so the UI never round-trips secrets.
- The issued private keys are written `0600`, and the managed cert dir is added to
  `.gitignore`.

### 2.7 Tests / verification

- `minica_client` unit tests against a stubbed HTTP server (auth header, error mapping).
- `cert_manager` tests: renewal decision (expiring vs. fresh certs, boundary at
  `renew_before_days`), atomic replace, CN/SAN-change triggers reissue, startup pass is
  non-blocking (listener with stale-but-present material starts immediately), hot-swap
  (connection accepted after swap sees the new cert; connection opened before swap completes
  on the old one).
- Config tests: listener configs without the new fields load unchanged (`cert_source`
  defaults to `manual`); removed local-CA admin fields fail startup with the migration
  message.
- Manual end-to-end against a real minica instance: configure CA → test → terminate listener
  issued cert → force-renew → confirm new cert is served without dropping an open connection.

### 2.8 Decisions (resolved 2026-07-07)

1. **Hot-swap from v1** — renewals swap cert material via ArcSwap without dropping proxy
   connections. (Admin server: rocket's TLS is fixed at launch, so admin-cert renewal
   restarts the admin server only — see 2.3.)
2. **`renew_before_days` is per-CA** config.
3. **Local CA removed** — `CA.pem`/`CA.key` generation is deleted; the admin server's cert is
   issued from a configured minica CA with SANs from the admin server's configured
   hostnames/IPs.
4. **Renewal job triggers**: non-blocking startup pass (existing material serves until a
   replacement lands), daily tick, and on-demand from the admin UI.

---

## 3. (next item — to be added)
