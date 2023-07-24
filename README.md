# tlsproxy_rs
TLS Proxy written in Rust

It listens on host port and forward to target SNI Host port. Supports almost all major TLS based protocols.


# Build

To build static binary (no glic requirement)
```bash
$ sh build.sh
```

To build normally
```
$ cargo build --release
```

# Running
```bash
$ target/x86_64-unknown-linux-musl/release/tlsproxy -b 0.0.0.0:443:443 -b 0.0.0.0:3465:465
```

It starts 2 listeners:

* one listens on 443, and forward the requests to SNI hosts port 443
* one listens on 3465, and forward the request to SNI hosts port 465

# Other options
You may get help by using `-h` or `--help` on the compiled binary.

```bash
$ target/x86_64-unknown-linux-musl/release/tlsproxy --help

Usage: tlsproxy [OPTIONS]

Options:
  -b, --bind <BIND>                    forward config `bind_ip:bind_port:forward_port` format (repeat for multiple)
  -r, --ri <RI>                        stats report interval in ms [default: 30000]
      --log-conf-file <LOG_CONF_FILE>  log4rs config yaml file path [default: log4rs.yaml]
      --self-ip <SELF_IP>              self IP address to reject (repeat for multiple)
      --acl <ACL>                      acl files. see `rules.json` [default: ]
      --max-idle <MAX_IDLE>            close connection after max idle in seconds [default: 300]
      --resolve-conf <RESOLVE_CONF>    special port resolve conf [default: ]
  -h, --help                           Print help

```

Note: idle is defined as the duration that both direction had no traffic (no bytes written successfully).

# Avoid connecting to self
The program will cause infinite loop if it connects to itself. To avoid that, you can specify a self ip by
`--self-ip "ip1" --self-ip "ip2"`. 

When SNI info points to host that would resolve to one of the self IP addresses, the connection will be rejected.

# ACL
You can use `-acl rule.json` to specify a ACL for host check. 
This will help you to limit the target host names by static check, or by regular expression.

See `rules.json` in the repo for more info.

Example

```json
{
    "no_match_decision":"reject",
    "whitelist":[
        "host:a",
        "host:www.google.com",
        "pattern:www\\.goo.*\\.com"
    ],
    "blacklist":[
        "host:a",
        "host:b",
        "pattern:www.goddogle.com",
        "$any"
    ]
}
```

The rule says: When no whitelist/blacklist is matching the host, the decision will be "reject" (valid options are "accept", "reject"),

whitelist and blacklist are hosts that are allowed or rejected. 

All checks are case insensitive.

`host:xxx` is valid just for host xxx using exact match.
`pattern:xxx` is valid if regular expression xxx matches the host name to connect. It also matches xxxxx, xxxxxx....
If you want to be exactly match by pattern, either use `host:xxxxx`, or `pattern:^xxxxx$`.
`$any` is valid for all hosts. It is more efficient thatn `pattern:^.*$` because it requires no matching.

It would set the whole rule to instant `true` without matching attempt. It superseds all sibling rules.

# Logging
The software uses log4rs. A handy log4rs.yaml is given by default. Specify the log config file by using
`--log-conf-file`. The default config file is `./log4rs.yaml`.

# Resolve override
The TLS Proxy supports special resolve override. The purpose of resolve override is to redirect TLSHost & Port to another host.

To set Resolve Override, specify `--resolve-conf some-rule.json` in command line.

An example of resolve configuration JSON file is as follows:

```json
{
    "www.google.com;www.g1.com;www.g2.com;www.g3.com:3": "127.0.0.1",
    "www.baidu.comP465": "127.0.0.1:465",
    "www.google.com:443": "192.168.44.17"
}
```

You can define multiple hosts to map to the same target host.

When source host has no port, it implies all ports.

When destination has no port, it implies same as lookup source port.

Look up happens for specific host + specific port first, if not found, fall back to host only lookup.

When connect connects to your services and try to send TLS header to connect to the above hosts in `from` field, the actual attempted host is determined by the `to` field.

Note about the order: 

* Initial port mapping is done first. Your `0.0.0.0:443:1443` (`1443` is the mapped port)
* Special host resolution (resolve-conf) is performed second (after mapped port).
* ACL is evaluated third using resolved `HostAndPort` (after special host resolution done)
* Actual DNS lookup is performed, self IP address is checked
* TLS Proxy piping is done at last

NOTE: Both `from.port` and `to.port` can be skipped. If `from.port` is skipped, it is for all ports, except those explicitly configured.

If `to.port` is skipped, it is the mapped port. 

# Enjoy

