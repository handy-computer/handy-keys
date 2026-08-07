//! Diagnostic event dump for hardware characterization and bug reports.
//!
//! Prints every event the `KeyboardListener` emits, with millisecond
//! timestamps and raw modifier bits. Use this to answer questions like
//! "does F13 carry the Fn modifier on this keyboard?" or "what does this
//! layout report for the key next to 1?" — and paste the output into
//! bug reports.
//!
//! Run with: cargo run --example diagnostic
//! Exit with Ctrl+C.
//!
//! Notes:
//! - Keys the platform backend cannot map are invisible here (no event is
//!   emitted for them). If a key produces no output, that itself is a
//!   finding — note it.
//! - macOS requires accessibility permission for the terminal running this.
//! - Left/right mouse buttons are only reported while a modifier is held
//!   (by design, to avoid noise).

use std::time::Instant;

use handy_keys::KeyboardListener;

// Ctrl+Pause arrives as Ctrl+Break in a Windows console, and the default handler
// terminates the process. Swallow CTRL_BREAK_EVENT so the key reaches the hook.
#[cfg(target_os = "windows")]
fn prevent_ctrl_break_termination() {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT};

    unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
        BOOL::from(ctrl_type == CTRL_BREAK_EVENT)
    }

    if let Err(e) = unsafe { SetConsoleCtrlHandler(Some(handler), true) } {
        eprintln!("warning: could not install console Ctrl+Break handler: {e}");
    }
}

fn main() -> handy_keys::Result<()> {
    println!(
        "handy-keys {} diagnostic — os={} arch={}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    println!("Press keys to dump events. Ctrl+C to exit.");
    println!();

    #[cfg(target_os = "windows")]
    prevent_ctrl_break_termination();

    let listener = KeyboardListener::new()?;
    let start = Instant::now();

    while let Ok(event) = listener.recv() {
        let state = if event.is_key_down { "DOWN" } else { "UP  " };
        let mods = if event.modifiers.is_empty() {
            "(none)".to_string()
        } else {
            event.modifiers.to_string()
        };
        let changed = match event.changed_modifier {
            Some(m) => format!(" changed={}", m),
            None => String::new(),
        };

        println!(
            "[+{:>8}ms] {} key={:<16} mods={} (bits=0x{:03x}){}",
            start.elapsed().as_millis(),
            state,
            event
                .key
                .map(|k| format!("{:?}", k))
                .unwrap_or_else(|| "-".to_string()),
            mods,
            event.modifiers.bits(),
            changed,
        );
    }

    Ok(())
}
