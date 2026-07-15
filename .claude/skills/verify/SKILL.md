---
name: verify
description: Build, run, and drive tlsproxy to verify changes end-to-end
---

# Verifying tlsproxy changes

Build: `cargo build` (binary at `target/debug/tlsproxy`).

Run: `tlsproxy run -c config.yaml` — logs to stdout, `RUST_LOG` overrides
the `info` filter. Also `tlsproxy validate -c file` (config load check,
exit 1 on error) and `tlsproxy genconfig`.

Minimal config (write to a scratch dir; `runtime_dir` is created relative
to the cwd and a local CA is generated inside it):

```yaml
listeners:
  web:
    bind: 127.0.0.1:15443   # or "%lo%:15443" interface pattern
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options:
  runtime_dir: ./runtime
dns: {}
admin_server:
  bind_address: 127.0.0.1
  bind_port: 15888
  username: admin
  password: admin
  tls: false
```

Drive it:
- `ss -tlnp | grep <port>` confirms what actually bound.
- Admin UI: `curl -u admin:admin http://127.0.0.1:15888/` → 200.
- Startup success/failure is in the log: `starting manager: <name> started
  OK` vs `<name> start failed (<reason>)`. A failed listener does not kill
  the process — check the log, not the exit code.
- Bind failures retry 3 times at 100ms before reporting.
