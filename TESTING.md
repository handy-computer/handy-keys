# Testing

## 1. Unit tests

```sh
cargo test
```

Covers the platform-independent logic: hotkey matching, modifier
semantics, string parsing, and the manager's press/release state machine.
Always run this; it must stay green on every commit.

## 2. Cross-target check

```sh
./scripts/check-all.sh
```

Runs the unit tests on the host, then `cargo check --all-targets` for the
other supported platforms so platform-specific compile breakage is caught
without sitting at that machine. Needs the target stdlibs once:

```sh
rustup target add x86_64-pc-windows-msvc    # from macOS/Linux
rustup target add aarch64-apple-darwin      # from other hosts
```

Known limitation: the Linux target cannot be cross-checked because rdev's
`evdev-sys` dependency builds C libevdev via autotools. Until the rdev
backend is replaced with a pure-Rust evdev implementation, Linux compile
errors only surface when building on a Linux machine (which additionally
needs autotools installed for that same dependency).

## 3. Integration tests — on the target machine

```sh
cargo test --test synthetic_input -- --ignored
```

Drives the real platform backend end-to-end by injecting synthetic input
through the OS and asserting on the events the listener emits. `#[ignore]`d
by default because they need a real interactive session.

- **macOS**: requires accessibility permission for the terminal running
  the tests (System Settings > Privacy & Security > Accessibility).
  Injection uses `CGEventPost`.
- **Windows**: requires an interactive desktop session. Injection uses
  `SendInput`.
- **Linux**: no harness yet — the rdev backend exclusively grabs real
  input devices, which is unsafe to drive from a test. A uinput-based
  harness (virtual keyboard, indistinguishable from hardware at the evdev
  layer) lands with the in-tree evdev backend.

The tests inject **F20** only: it exists in the `Key` enum everywhere and
does nothing in terminals or desktop environments, so a test run never
types into the session it runs in.

## Manual characterization — `diagnostic` example

```sh
cargo run --example diagnostic
```

Dumps every event the listener emits (timestamps, key, modifiers, raw
modifier bits). Use it for hardware characterization sessions ("does F13
carry Fn on this keyboard?", "what does this layout report for the key
next to 1?") and ask bug reporters to paste its output. A key that
produces *no* output is itself a finding — it means the platform backend
cannot map it.
