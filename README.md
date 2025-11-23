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
$ cargo build --release
```

# Running
## Directory structure

You should build your tls proxy using `$ cargo build --release`

And copy `target/release/tlsproxy` to a separate folder.

In the same folder, you should also copy the following files:

- static/
- config.yaml
- log4rs.yaml

If you want to enable TLS, please create your `server.pem`, `server.key` and `ca.pem`.

You may refer to https://github.com/wushilin/minica for cert management.

## Prepare configuration

Example config.yaml
```yaml
listeners:
  HTTPS:
    bind: 0.0.0.0:1443  # TLS proxy binds to 0.0.0.0:1443
    target_port: 443  # Proxy all requests to port 443
    policy: DENY  # If rules matched, they will be denied (possible values: ALLOW|DENY). basically the rules is blacklist. If it is ALLOW, it would act as a whitelist
    rules:
      static_hosts: []  # Static host matching, ignore case
      patterns:
      - ^www.g.*$  # Regex checking, case sensitive. You probably want to add (?i) if you want to ignore case...
    max_idle_time_ms: 3600000
    speed_limit: 0.0 # Speed limit for each connection. 0 is no limit. unit is bytes/second, shared by upload/download together
options:
  log_config_file: log4rs.yaml  # log4rs config
  self_ips:
  - 127.0.0.1  # If target server resolves to this host it will be rejected
  - me.jungle # If target server resolves to one of the addresses by this domain, it will be rejected
dns:
  # DNS override configuration
  # You can resolve DNS in 3 ways:
  #
  # 1. Explicit matching (highest priority):
  #    github.com:443 -> my.local.host:443
  #    Matches exact hostname:port combinations
  #
  # 2. Suffix matching (medium priority):
  #    suffix:thub.com:443 -> my.local.host:443
  #    Matches hostnames ending with the specified suffix
  #    Longer suffixes have higher priority than shorter ones
  #
  # 3. Regex matching (lowest priority):
  #    regex:.*thub.com:443 -> my.local.host:443
  #    Matches hostnames using regular expressions
  #    Tried in order of their definition when explicit and suffix don't match
  #
  # Priority order:
  # 1. Explicit matches are checked first (highest priority)
  # 2. Then suffix matches (longer suffixes checked before shorter ones)
  # 3. Finally regex matches are tried in order of definition
  #
  # Example:
  home.wushilin.net: 192.168.44.100 # Explicit match
  # suffix:thub.com:443 -> my.local.host:443 # Suffix match example
  # regex:.*thub.com:443 -> my.local.host:443 # Regex match example
# If you only service requests from trusted network, then you can disable IP check
# By Default, the service refuses to connect to any local address, unless
# it is defined in the dns override earlier
# Also, it refuses connection came from local addresses (e.g. ::1, 127.0.0.1) 
# to prevent circular connection, that check can be disabled by setting
# disable_check_ip -> true
disable_check_ip: false
admin_server:
  bind_address: 0.0.0.0  # Admin server bind to this address
  bind_port: 48889 # Admin server bind to this port
  username: admin # Admin server requires the basic user
  password: pass1234 # Admin server require the basic password
  tls_cert: null # TLS cert for admin server, required if tls => true
  tls_key: null # TLS key for admin server, required if tls => true
  tls_ca_cert: null # TLS CA cert for admin server, required if tls => true 
  mutual_tls: null # Enable mtls or not true|false|null. If set to true, tls must also be true!
  tls: false # Enable TLS or not
  rocket_log_level: critical # Rocket log level

```

Sample log4rs.yaml
```yaml
refresh_rate: 60 seconds

appenders:
  stdout:
    kind: console
  default:
    kind: rolling_file
    path: "tlsproxy.log"
    append: true
    encoder:
      pattern: "{d(%Y-%m-%d %H:%M:%S%.3f %Z)} {M} {({l}):5.5} {f}:{L} - {m}{n}"
    policy:
      kind: compound
      trigger:
        kind: size
        limit: 10 mb
      roller:
        kind: fixed_window
        pattern: "tlsproxy.{}.log.gz"
        count: 20
        base: 1
root:
  level: info
  appenders:
    - default
    - stdout

loggers:
  tlsproxy:
    level: info
    appenders:
      - default
      - stdout
    additive: false
```

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

Visit your server at http://host:48889 to start managing.

If prompted for Basic auth, please enter the username and password

# Enjoy
