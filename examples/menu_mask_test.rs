//! Live test for the Win/Alt menu-mask injection (PR #30, cjpais/Handy#917).
//!
//! Registers three BLOCKING hotkeys and prints every hotkey event:
//!
//!   1. Win+Space        — the #917 repro
//!   2. Alt+H            — Alt sibling (menu-bar focus heuristic)
//!   3. Win+Shift (mod-only)
//!
//! Run from an INTERACTIVE desktop terminal (not SSH/Session 0):
//!   cargo run --example menu_mask_test > mask-test.txt
//!
//! What to check while it runs:
//!   - Press+release Win+Space: hotkey fires, Windows search/Start does NOT
//!     open when Win is released, and Space does not reach the foreground app.
//!   - Focus Notepad/Explorer, press+release Alt+H: hotkey fires and the
//!     menu bar / ribbon does NOT get focused when Alt is released.
//!   - Press Win, then Shift, release both: hotkey fires, Start does NOT open.
//!   - Sanity: a lone Win tap still opens Start; Win+R still opens Run.
//!   - Win+L to lock, unlock, then repeat the Win+Space test (mask flag must
//!     re-arm after the secure-desktop transition).
//!
//! Ctrl+C to exit.

use handy_keys::{Hotkey, HotkeyManager, Key, Modifiers};

fn main() -> handy_keys::Result<()> {
    let manager = HotkeyManager::new_with_blocking()?;

    let hotkeys = [
        ("Win+Space", Hotkey::new(Modifiers::CMD, Key::Space)?),
        ("Alt+H", Hotkey::new(Modifiers::OPT, Key::H)?),
        (
            "Win+Shift",
            Hotkey::new(Modifiers::CMD | Modifiers::SHIFT, None)?,
        ),
    ];

    println!("menu_mask_test — blocking hotkeys registered:");
    let mut names = std::collections::HashMap::new();
    for (name, hk) in hotkeys {
        let id = manager.register(hk)?;
        names.insert(id, name);
        println!("  {name}");
    }
    println!("Ctrl+C to exit.\n");

    while let Ok(event) = manager.recv() {
        println!("{:?} {}", event.state, names[&event.id]);
    }
    Ok(())
}
