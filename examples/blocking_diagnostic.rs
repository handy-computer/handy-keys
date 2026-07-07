//! Interactive manual test for hotkey *blocking*.
//!
//! Spawns a blocking `KeyboardListener` and dumps every event it sees,
//! marking the ones that are being withheld from other applications.
//! On Linux this exercises the full grab + uinput re-injection pipeline:
//! while it runs, every keystroke on every keyboard flows through the
//! per-device clones.
//!
//! ```sh
//! cargo run --example blocking_diagnostic              # blocks "z"
//! cargo run --example blocking_diagnostic ctrl+space   # blocks Ctrl+Space
//! cargo run --example blocking_diagnostic z f9 cmd+d   # several at once
//! ```
//!
//! Exits automatically after 90 seconds as a safety net (Ctrl+C works too:
//! unblocked keystrokes keep reaching the terminal through the clones).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use handy_keys::{BlockingHotkeys, Error, Hotkey, KeyboardListener};

const RUN_FOR: Duration = Duration::from_secs(90);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let specs: Vec<String> = if args.is_empty() {
        vec!["z".to_string()]
    } else {
        args
    };

    let mut set = HashSet::new();
    for spec in &specs {
        match spec.parse::<Hotkey>() {
            Ok(hotkey) => {
                set.insert(hotkey);
            }
            Err(e) => {
                eprintln!("cannot parse hotkey '{spec}': {e}");
                std::process::exit(2);
            }
        }
    }

    println!("blocking: {}", specs.join(", "));
    println!();
    println!("While this runs, check:");
    println!("  1. Type normally in another window — latency and autorepeat should feel");
    println!("     unchanged (everything is re-injected through uinput clones right now).");
    println!(
        "  2. Press the blocked key(s) [{}] in a text field — nothing should be typed",
        specs.join(", ")
    );
    println!("     there, but the event prints below marked [BLOCKED].");
    println!("  3. Toggle CapsLock — the light must still work; the [LED] line tracks it.");
    println!("  4. Hold a key across exit — nothing should stick afterwards.");
    println!();
    println!(
        "Auto-exit after {}s; Ctrl+C to stop sooner.",
        RUN_FOR.as_secs()
    );
    println!();

    let hotkeys: BlockingHotkeys = Arc::new(Mutex::new(set));
    let listener = match KeyboardListener::new_with_blocking(Arc::clone(&hotkeys)) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("failed to start blocking listener: {e}");
            std::process::exit(1);
        }
    };
    println!("listener running — keyboards grabbed\n");

    std::thread::spawn(watch_capslock_led);

    let deadline = Instant::now() + RUN_FOR;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match listener.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(event) => {
                // Same predicate the backend uses, so the marker reflects
                // what other applications actually did not receive.
                let blocked = hotkeys
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|h| h.modifiers.matches(event.modifiers) && h.key == event.key);

                let direction = if event.is_key_down { "down" } else { " up " };
                let what = match (event.key, event.changed_modifier) {
                    (Some(key), _) => format!("{key}"),
                    (None, Some(modifier)) => format!("<{modifier:?}>"),
                    (None, None) => "?".to_string(),
                };
                println!(
                    "{direction}  {what:<20} mods={:?}{}",
                    event.modifiers,
                    if blocked { "   [BLOCKED]" } else { "" }
                );
            }
            Err(Error::Timeout) => {} // just re-check the deadline
            Err(e) => {
                eprintln!("listener stopped: {e}");
                break;
            }
        }
    }

    println!("\ndropping listener (releasing grabs and clones)…");
    drop(listener);
    println!("done — input should be fully back to normal");
}

/// Print CapsLock LED state changes, read from sysfs. Settles the "do lock
/// LEDs still update while the keyboard is grabbed?" question empirically.
fn watch_capslock_led() {
    let mut brightness_files = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/leds") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().contains("capslock") {
                brightness_files.push(entry.path().join("brightness"));
            }
        }
    }
    if brightness_files.is_empty() {
        println!("   [LED] no capslock LED under /sys/class/leds — skipping LED watch");
        return;
    }

    let mut last: Option<bool> = None;
    loop {
        let on = brightness_files.iter().any(|path| {
            std::fs::read_to_string(path)
                .map(|s| s.trim() != "0")
                .unwrap_or(false)
        });
        if last != Some(on) {
            println!("   [LED] CapsLock light: {}", if on { "ON" } else { "OFF" });
            last = Some(on);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
