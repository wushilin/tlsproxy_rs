# Backend address validation

## Problem

Backend addresses use a different syntax per listener mode, and entering the
wrong one fails silently or with an unhelpful message.

- `forward` mode expects a plaintext `host:port`.
- `http` mode expects `http://host` or `http://host:port`.

Typing `http://backend-ip:port` for a forward (TLS to plaintext) listener, or a
bare `host:port` for an HTTP reverse proxy listener, is an easy mistake.

### Root cause on the server

`HostAndPort`'s parser (`src/hostutil.rs:14`) matches `^\s*(\S+)\s*:\s*(\d+)\s*$`.
`http://10.0.0.5:8080` therefore parses successfully, yielding host
`http://10.0.0.5` and port `8080`. `parse_targets` (`src/forward/mod.rs:705`)
accepts it, DNS resolution of the bogus host fails at connect time, and the
backend simply shows as offline with no indication that the address was
malformed.

`Config::validate` (`src/config.rs:235`) compounds this by validating only
`http`-mode targets, so a malformed forward target is never caught at load.

### Gaps in the web UI

`validateListener` (`static/app.js:874`) does check both modes, but:

- it only runs on dialog save, surfacing as a toast;
- the forward-mode message ("has an invalid forward target") does not say what
  is wrong or what the correct form is;
- the rules are inline regexes duplicating the Rust logic, free to drift from it;
- `cleanListener` (`static/app.js:923`) persists `target` for `terminate` and
  `passthrough`, where the field is hidden and the value is unused, so a stale
  value survives a mode switch unvalidated.

## Design

### 1. Reject schemes in `parse_targets`

Add an explicit scheme check before `HostAndPort` parsing, so the greedy regex
cannot absorb `http://` into the host:

```
forward target `http://10.0.0.5:8080` must be a plaintext host:port with no
scheme — try `10.0.0.5:8080`
```

Any `scheme://` prefix is rejected the same way. `parse_http_targets`
(`src/forward/mod.rs:330`) already handles the mirror case and is unchanged.

### 2. Validate forward targets at config load

`Config::validate` gains a `ListenerMode::Forward` arm calling `parse_targets`,
which also brings its existing "forward mode requires at least one target"
check to load time. `parse_targets` becomes `pub(crate)`.

This is deliberately stricter than current behaviour: a config with a malformed
forward target loads today and fails per-connection, but after this change fails
the whole config load. Accepted by the user as intended.

### 3. Shared per-mode validator in the UI

A single `validateTargets(mode, text)` in `static/app.js` returns `{ok: true}` or
`{ok: false, message}`, mirroring the Rust rules:

| Mode | Expected | On mismatch |
| --- | --- | --- |
| `forward` | `host:port` | scheme present: "remove the `http://` scheme", with a corrected example derived from the input |
| `http` | `http://host[:port]`, no path | bare `host:port`: "add the `http://` scheme"; `https://`: "https backends are not supported" |
| `terminate`, `passthrough` | unused | nothing to validate |

`validateListener` delegates to it, replacing its two inline regex blocks, so
the save-time toast and the inline hint cannot disagree.

### 4. Inline feedback, save blocked

A new `<p class="error" id="listener-target-error">` sits under the Targets
helper in `static/index.html`. It is driven by the `input` event on
`#listener-target` and by `syncListenerDialogMode()`, so switching mode
re-validates immediately — a value valid for `forward` is invalid for `http`.
It toggles `aria-invalid` on the textarea and disables the dialog's Save button
while invalid. The existing on-save toast remains as the backstop.

### 5. Drop stale targets

`cleanListener` emits an empty `target` for `terminate` and `passthrough`, so a
value typed under `forward` cannot be persisted as dead config on a mode that
ignores it. The textarea retains its text while the dialog is open, so toggling
modes does not lose typing.

## Testing

- Rust unit tests in `src/config.rs`, alongside the existing
  `http_listener_rejects_bad_backends_at_load`: a scheme-prefixed forward target
  is rejected at load, and a valid target list is accepted.
- Rust unit tests for `parse_targets` in `src/forward/mod.rs` covering
  `http://`, `https://`, and other `scheme://` prefixes.
- The repository has no JavaScript test harness (no `package.json`), so the UI
  half is verified by driving the running app.
