//! Automated live check for the Win menu-mask injection (PR #30).
//!
//! Two phases, both injecting input via SendInput on the interactive desktop:
//!
//!   1. CONTROL: a lone Win tap. Start must OPEN (proves the foreground-window
//!      detector works). Esc is injected to close it again.
//!   2. MASKED: Win down → F20 down/up (blocked Win+F20 hotkey) → Win up.
//!      With the mask fix, Start must NOT open.
//!
//! Run from an interactive desktop session:
//!   cargo run --example mask_live_check
//!
//! Exits 0 on pass, 1 on fail. Expect the Start menu to flash once (control).

#[cfg(not(windows))]
fn main() {
    eprintln!("mask_live_check is Windows-only");
}

#[cfg(windows)]
use std::thread::sleep;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use handy_keys::{Hotkey, HotkeyManager, Key, Modifiers};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_ESCAPE, VK_F20, VK_LWIN,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};

#[cfg(windows)]
fn inject(vk: VIRTUAL_KEY, up: bool) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    assert_eq!(sent, 1, "SendInput failed");
    sleep(Duration::from_millis(60));
}

#[cfg(windows)]
fn foreground() -> (usize, String) {
    let hwnd = unsafe { GetForegroundWindow() };
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    (hwnd.0 as usize, String::from_utf16_lossy(&buf[..len]))
}

#[cfg(windows)]
fn main() -> handy_keys::Result<()> {
    let manager = HotkeyManager::new_with_blocking()?;
    manager.register(Hotkey::new(Modifiers::CMD, Key::F20)?)?;
    sleep(Duration::from_millis(300)); // let the hook thread install

    // Phase 1 — control: lone Win tap must open Start.
    let before = foreground();
    inject(VK_LWIN, false);
    inject(VK_LWIN, true);
    sleep(Duration::from_millis(1200));
    let after = foreground();
    let control_opened = after.0 != before.0;
    println!(
        "control: foreground {} -> {} ({}) — Start {}",
        before.1,
        after.1,
        if control_opened {
            "changed"
        } else {
            "unchanged"
        },
        if control_opened {
            "OPENED as expected"
        } else {
            "did NOT open (detector blind?)"
        },
    );
    if control_opened {
        inject(VK_ESCAPE, false);
        inject(VK_ESCAPE, true);
        sleep(Duration::from_millis(800));
    }

    // Phase 2 — masked: blocked Win+F20, then Win release.
    let before = foreground();
    inject(VK_LWIN, false);
    inject(VK_F20, false);
    inject(VK_F20, true);
    inject(VK_LWIN, true);
    sleep(Duration::from_millis(1200));
    let after = foreground();
    let masked_opened = after.0 != before.0;
    println!(
        "masked:  foreground {} -> {} ({}) — Start {}",
        before.1,
        after.1,
        if masked_opened {
            "changed"
        } else {
            "unchanged"
        },
        if masked_opened {
            "OPENED (mask failed)"
        } else {
            "stayed closed"
        },
    );

    // Phase 3 — key-up edge: F20 pressed BEFORE Win, released while Win is
    // held. The F20-up is blocked (matches Win+F20) and its down passed
    // before the Win hold began, so without an up-mask the shell sees a lone
    // Win tap and opens Start.
    let before = foreground();
    inject(VK_F20, false); // passes through: no modifier held yet
    inject(VK_LWIN, false);
    inject(VK_F20, true); // blocked: Win+F20 matches on the up
    inject(VK_LWIN, true);
    sleep(Duration::from_millis(1200));
    let after = foreground();
    let edge_opened = after.0 != before.0;
    println!(
        "edge:    foreground {} -> {} ({}) — Start {}",
        before.1,
        after.1,
        if edge_opened { "changed" } else { "unchanged" },
        if edge_opened {
            "OPENED (key-up edge is real)"
        } else {
            "stayed closed"
        },
    );
    if edge_opened {
        inject(VK_ESCAPE, false);
        inject(VK_ESCAPE, true);
        sleep(Duration::from_millis(800));
    }

    let mut fired = Vec::new();
    while let Some(event) = manager.try_recv() {
        fired.push(format!("{:?}", event.state));
    }
    println!("hotkey events observed: {:?}", fired);
    if masked_opened {
        inject(VK_ESCAPE, false);
        inject(VK_ESCAPE, true);
    }

    let hotkey_fired = fired.iter().any(|s| s == "Pressed");
    let pass = control_opened && !masked_opened && !edge_opened && hotkey_fired;
    println!("{}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
