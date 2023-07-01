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

# Avoid connecting to self
The program will cause infinite loop if it connects to itself. To avoid that, you can specify a self ip by
--self-ip "ip1" --self-ip "ip2". 

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
        "pattern:www.goddogle.com"
    ]
}
```

The rule says: When no whitelist/blacklist is matching the host, the decision will be "reject" (valid options are "accept", "reject"),

whitelist and blacklist are hosts that are allowed or rejected. 

All checks are case insensitive.

`host:xxx` is valid just for host xxx using exact match.
`pattern:xxx` is valid if regular expression xxx matches the host name to connect. It also matches xxxxx, xxxxxx....
If you want to be exactly match by pattern, either use `host:xxxxx`, or `pattern:^xxxxx$`.



# Enjoy
