# TLS Proxy with a UI

# Configuration UI
Online edit via Admin server
![image](https://github.com/wushilin/tlsproxy_rs/assets/7019828/b2b1ae48-685b-4ee1-a820-3515cefb1b64)


# Online configuration change
![image](https://github.com/wushilin/tlsproxy_rs/assets/7019828/63af4d86-2f45-4ad7-99c1-ca2935a1dd5a)

# DNS Override
![image](https://github.com/wushilin/tlsproxy_rs/assets/7019828/9046871b-b38c-46e8-aadc-ea2aa1816da1)


You may update the server config via web UI and trigger restart.

# Realtime monitoring and statistics
Visualize the listener and tls proxy status

![image](https://github.com/wushilin/tlsproxy_rs/assets/7019828/264a1008-c13c-48bc-aba5-3a26ef88f0d4)


Supports

- Total connection
- Active connection
- Uploaded bytes
- Downloaded bytes



# Building

```bash
$ sh build.sh
```

The management console is dependency-free and embedded into the release binary
at compile time. It does not require Node.js or runtime Internet access.
By default, `build.sh` builds the musl release binary at
`target/x86_64-unknown-linux-musl/release/tlsproxy`. Override with
`TARGET=... sh build.sh` if you need a different Rust target.

# Running
```bash
$ tlsproxy run -c config.yaml
```

When no subcommand is supplied, `tlsproxy` defaults to `tlsproxy run -c
config.yaml`. Use `tlsproxy genconfig -c config.yaml` to create a starter
configuration; it refuses to overwrite an existing file. Use `tlsproxy validate
-c config.yaml` to parse-check a configuration before running it.

## Directory structure

You should build your tls proxy using `$ sh build.sh`

And copy `target/x86_64-unknown-linux-musl/release/tlsproxy` to a separate folder.

In the same folder, you should also copy the following files:

- config.yaml

When admin TLS or listener TLS termination is enabled, tlsproxy creates or
reuses a local CA under `runtime_dir`, then signs certificates with that CA.
Keep the CA key private and install `CA.pem` in clients that should trust the
generated certificates.

## Prepare configuration

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

Example config.yaml
```yaml
listeners:
  HTTPS:
    bind: 0.0.0.0:1443  # TLS proxy binds to 0.0.0.0:1443
    mode: passthrough # passthrough forwards TLS unchanged; terminate decrypts TLS; forward is a plain L4 port forward; http routes plaintext HTTP by Host header
    upstream_tls: false # terminate mode only; encrypt upstream without authenticating it
    target_port: 443  # Proxy all requests to port 443
    policy: DENY  # If rules matched, they will be denied (possible values: ALLOW|DENY). basically the rules is blacklist. If it is ALLOW, it would act as a whitelist
    rules:
      static_hosts: []  # Static host matching, ignore case
      patterns:
      - ^www.g.*$  # Regex checking, case sensitive. You probably want to add (?i) if you want to ignore case...
    max_idle_time_ms: 3600000
    speed_limit: 0.0 # Speed limit for each connection. 0 is no limit. unit is bytes/second, shared by upload/download together
options:
  runtime_dir: ./runtime  # runtime artifacts; local CA data lives under this directory
dns:
  # DNS overrides, applied to the SNI hostname before connecting upstream.
  # Three rule kinds, each in its own section. Resolution priority (first hit
  # wins):
  #   1. exact host:port
  #   2. exact host (any port)
  #   3. suffix rules with a port (longest suffix first)
  #   4. suffix rules without a port (longest suffix first)
  #   5. regex rules, in definition order
  # If nothing matches, normal DNS applies. In every rule, a `to` without a
  # port keeps the port that would have been used.
  exact:
  - from: home.wushilin.net        # no port = matches any port
    to: 192.168.44.100
  # - from: github.com:443         # host:port = that port only
  #   to: my.local.host:443
  suffix:
  # Matches the domain and all its subdomains at label boundaries:
  # `.abc.com` matches `x.abc.com` and `abc.com`, but never `notabc.com`.
  # - from: .internal.abc.com:443
  #   to: 127.0.0.1:443
  regex:
  # Case-insensitive, matched against the HOSTNAME ONLY — the port is never
  # part of the text the pattern sees. Use `port` to restrict the rule.
  # - hostname: '^api\d+\.abc\.com$'
  #   port: 443                     # optional; omit to match any port
  #   to: 127.0.0.1:443
# Self-connection loops (the proxy connecting back to itself, directly or
# through NAT/port-forwarding) are detected automatically: the proxy remembers
# the TLS ClientHello randoms it forwarded in the last 10 seconds and closes
# any inbound connection presenting one of them. No configuration is needed.
# Local CA for every certificate the proxy manages: the admin server
# certificate and on-demand terminating-listener certificates. When the
# section is absent, these defaults are used.
#
# Paths are always resolved under {runtime_dir}. Ad-hoc SNI certificates are
# cached in memory only. The admin certificate is cached in memory, loaded from
# disk on restart, immediately evicted if within 72 hours of expiry, and saved
# after lazy renewal. All generated leaf certificates are valid for 365 days.
ca:
  localca:
    ca_cert: local_ca/CA.pem
    ca_key: local_ca/CA.key
    working_dir: local_ca
admin_server:
  bind_address: 0.0.0.0  # Admin server bind to this address
  bind_port: 48888 # Admin server bind to this port
  username: admin # Admin server requires the basic user
  password: pass1234 # Admin server require the basic password
  tls: false # Enable TLS or not
  san: # Subject alternative names for the admin certificate (DNS names and IPs)
  - localhost
  - 127.0.0.1
  - ::1

```

## Accounting (CDR log)

When enabled, every client connection writes one CSV line on close, suitable
for usage accounting by SNI, listener, or target:

```yaml
accounting:
  enabled: true
  log_file: cdr.log
  rotate_size: 18MiB   # bytes, or KiB/MiB/GiB
  max_keep: 10         # rotated files kept
  compress_after: 3    # cdr.log.1..3 stay raw; cdr.log.4+ are compressed
  compression: zstd    # zstd (default) | gzip | none
```

Columns: `listener_type, connection_id, listener_name, sni, target_host,
target_endpoint, remote_address, status, uploaded_bytes, downloaded_bytes,
connection_start, connection_end`. `uploaded_bytes` counts bytes received from
the client, and `downloaded_bytes` counts bytes received from upstream.
`status` is `OK`, `DENIED` (ACL), or `CONNECT_FAILED`. Rotation is
logrotate-style (`.1` is newest); rotated files past `compress_after` are
compressed in-process. Records are written when a connection ends; connections
open when the process is killed are lost.

Terminating listeners mint a leaf certificate on demand for the exact SNI in
the client ClientHello, after the listener ACL allows that SNI. These ad-hoc
certificates are cached in memory and are not written to disk. With
`upstream_tls: true`, the proxy encrypts the upstream leg but does not
authenticate the upstream certificate, so it provides no MITM detection.

Logs are written to stdout so a process supervisor (systemd, processmaster,
etc.) can capture them. The default level is `info`; override it with the
standard `RUST_LOG` environment variable (e.g. `RUST_LOG=debug`).

Sample systemd unit file
```yaml
[Unit]
Description=The TLS Proxy
After=syslog.target network-online.target remote-fs.target nss-lookup.target
Wants=network-online.target
        
[Service]
Type=simple
WorkingDirectory=/opt/services/tlsproxy
PIDFile=/opt/services/tlsproxy/tlsproxy.pid
ExecStart=/opt/services/tlsproxy/tlsproxy
# ExecStop=/bin/kill -s QUIT $MAINPID
PrivateTmp=true
        
[Install]
WantedBy=multi-user.target
```

## Start

Just run the tlsproxy. No argument required. All support files must be in the same folder

Visit your server at http://host:48888 to start managing.

If prompted for Basic auth, please enter the username and password

# Enjoy
