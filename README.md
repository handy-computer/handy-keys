# handy-keys

Cross-platform global keyboard shortcuts library for Rust.

## Features

- **Cross-platform**: Works on macOS, Windows, and Linux
- **Global hotkeys**: Register system-wide keyboard shortcuts
- **Hotkey blocking**: Registered hotkeys are blocked from reaching other applications
- **Modifier-only hotkeys**: Support for shortcuts like `Cmd+Shift` without a key
- **String parsing**: Parse hotkeys from strings like `"Ctrl+Alt+Space"`
- **Hotkey recording**: Low-level keyboard listener for "record a hotkey" UI flows
- **Serde support**: All types implement `Serialize`/`Deserialize`

## Installation

```toml
[dependencies]
handy-keys = "0.1"
```

## Quick Start

```rust
use handy_keys::{HotkeyManager, Hotkey, Modifiers, Key};

fn main() -> handy_keys::Result<()> {
    let manager = HotkeyManager::new()?;

    // Register using the type-safe constructor
    let hotkey = Hotkey::new(Modifiers::CMD | Modifiers::SHIFT, Key::K)?;
    let id = manager.register(hotkey)?;

    // Or parse from a string
    let hotkey2: Hotkey = "Ctrl+Alt+Space".parse()?;
    manager.register(hotkey2)?;

    // Listen for events
    while let Ok(event) = manager.recv() {
        println!("Hotkey triggered: {:?}", event.id);
    }

    Ok(())
}
```

## Platform Notes

### macOS

Requires accessibility permissions. The library provides helpers to check and request access:

```rust
use handy_keys::{check_accessibility, open_accessibility_settings};

if !check_accessibility() {
    open_accessibility_settings()?;
}
```

### Windows

Uses low-level keyboard hooks. No special permissions required.

### Linux

Reads evdev devices (`/dev/input/event*`) directly, which works identically on
Wayland, X11, and the console — no display server connection is needed. Requires
read access to the device nodes, typically by adding your user to the `input`
group:

```sh
sudo usermod -aG input $USER   # then log out and back in
```

Keyboards plugged in while the listener is running are picked up automatically.

Hotkey **blocking** grabs keyboards exclusively and re-injects every
non-blocked keystroke through a per-device uinput clone, so it additionally
requires write access to `/dev/uinput` — for example via a udev rule:

```sh
echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | \
  sudo tee /etc/udev/rules.d/99-handy-keys-uinput.rules
sudo udevadm control --reload && sudo udevadm trigger /dev/uinput
```

Without it, the blocking constructors return an actionable error; the
non-blocking listener only needs `/dev/input` read access. Pointer devices are
never grabbed, so mouse-button hotkeys are detected but not blocked (matching
Windows).

Run at most one blocking listener per system: the first one owns the exclusive
grabs, and any further blocking listener (in the same process or another)
detects hotkeys but cannot block them.

### Shipping to Linux users

The setup above is fine for a development machine. When distributing an app,
don't ask users to join the `input` group — ship a udev **`uaccess`** rule
instead. systemd-logind then grants access to the *active seat user* (whoever
is physically logged in) through device ACLs: it takes effect immediately with
no logout, works the same on X11, Wayland, and the console, and unlike group
membership it does not extend to SSH sessions. One rule file covers both
listening and blocking:

```
# /usr/lib/udev/rules.d/70-yourapp-input.rules
# (uaccess rules must sort before 73-seat-late.rules, so keep the number < 73)
KERNEL=="uinput", TAG+="uaccess"
SUBSYSTEM=="input", KERNEL=="event*", TAG+="uaccess"
```

Deliver it one of two ways:

- **Packages (deb, rpm, AUR, ...)**: install the rule file from the package
  and reload udev in the post-install script — users never perform any setup:

  ```sh
  udevadm control --reload && udevadm trigger
  ```

- **AppImage and other installer-less formats**: the app cannot place the rule
  itself, so install it on first run. The constructors fail with actionable
  errors while access is missing; catch that, show an "enable keyboard access"
  button, and run a `pkexec` helper that writes the same rule file and runs the
  reload above. The ACL applies immediately, so retry the constructor without
  restarting the app.

Degrade gracefully where blocking is unavailable:

```rust
let manager = HotkeyManager::new_with_blocking() // blocks matched hotkeys
    .or_else(|_| HotkeyManager::new());          // read-only: detect but don't block
// If both fail, prompt the user to grant access (see above), then retry.
```

Be transparent with users about why the permission exists: read access to
`/dev/input` is keyboard-read capability — the kernel offers nothing
finer-grained than that. Sandboxed formats that hide `/dev/input` entirely
(e.g. Flatpak) cannot use this backend at all; they would need the XDG
GlobalShortcuts portal, which handy-keys does not currently implement.

## Modifiers

| Modifier | Aliases |
|----------|---------|
| `CMD` | `command`, `meta`, `super`, `win` |
| `CTRL` | `control` |
| `OPT` | `option`, `alt` |
| `SHIFT` | |
| `FN` | `function` (macOS only) |

## Recording Hotkeys

For implementing "press a key to set hotkey" UI:

```rust
use handy_keys::KeyboardListener;

let listener = KeyboardListener::new()?;

println!("Press a key combination...");
while let Ok(event) = listener.recv() {
    if event.is_key_down {
        if let Ok(hotkey) = event.as_hotkey() {
            println!("Recorded: {}", hotkey);
            break;
        }
    }
}
```

## License

MIT
