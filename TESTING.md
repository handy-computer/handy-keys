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
rustup target add x86_64-unknown-linux-gnu  # from other hosts
rustup target add aarch64-apple-darwin      # from other hosts
```

All three targets cross-check cleanly: every platform backend, including
the Linux evdev one, is pure Rust with no C build step.

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
- **Linux**: requires read access to `/dev/input` (`input` group
  membership) and write access to `/dev/uinput` (e.g.
  `sudo chmod 666 /dev/uinput` for the session, or a udev rule).
  Injection creates a uinput virtual keyboard — indistinguishable from
  hardware at the evdev layer — and covers the startup device scan,
  inotify hotplug, and hotkey blocking (a second listener asserts the
  blocked key never comes out of the grab-and-reinject pipeline).
  Note: the blocking test briefly grabs all keyboards, including real
  ones; keystrokes typed during it pass through the re-injection path.

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
