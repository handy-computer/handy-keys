//! Platform integration tests driven by synthetic input injection.
//!
//! These tests exercise the real platform backend end-to-end: they spawn a
//! `KeyboardListener`, inject input through the OS (`CGEventPost` on macOS,
//! `SendInput` on Windows), and assert on what comes out of the event
//! channel.
//!
//! They are `#[ignore]`d because they need a real interactive session (and
//! on macOS, accessibility permission for the process running the tests).
//! Run them on the target machine as part of the pre-merge checklist:
//!
//! ```sh
//! cargo test --test synthetic_input -- --ignored
//! ```
//!
//! Linux is intentionally absent: the current rdev backend requires an X11
//! display and exclusively grabs real input devices, which makes it unsafe
//! to drive from a test. A uinput-based harness (virtual keyboard device,
//! indistinguishable from hardware at the evdev layer) lands together with
//! the in-tree evdev backend that replaces rdev.
//!
//! All injections use F20: it exists in the `Key` enum on both platforms
//! and does nothing in terminals or the desktop environment, so a test run
//! never types into or otherwise disturbs the session it runs in.

#![cfg(any(target_os = "macos", target_os = "windows"))]

use std::time::{Duration, Instant};

use handy_keys::{Key, KeyboardListener};

/// Drain listener events until we see `key` in the given direction, or
/// time out. Other events (e.g. real user input during the run) are
/// skipped rather than failing the test.
fn saw_key_event(
    listener: &KeyboardListener,
    key: Key,
    is_key_down: bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match listener.recv_timeout(remaining) {
            Ok(event) => {
                if event.key == Some(key) && event.is_key_down == is_key_down {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
    false
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use objc2_core_graphics::{CGEvent, CGEventSource, CGEventSourceStateID, CGEventTapLocation};

    const VK_F20: u16 = 0x5A;

    fn post_f20(key_down: bool) {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
        let event = CGEvent::new_keyboard_event(source.as_deref(), VK_F20, key_down)
            .expect("failed to create keyboard event");
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    #[test]
    #[ignore = "needs accessibility permission; run: cargo test --test synthetic_input -- --ignored"]
    fn injected_f20_reaches_listener() {
        let listener = KeyboardListener::new().expect(
            "KeyboardListener::new failed — grant accessibility permission to the \
             terminal running this test (System Settings > Privacy & Security > Accessibility)",
        );

        post_f20(true);
        assert!(
            saw_key_event(&listener, Key::F20, true, Duration::from_secs(2)),
            "did not observe injected F20 key-down"
        );

        post_f20(false);
        assert!(
            saw_key_event(&listener, Key::F20, false, Duration::from_secs(2)),
            "did not observe injected F20 key-up"
        );
    }
}

#[cfg(target_os = "windows")]
mod windows_tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VK_F20,
    };

    fn send_f20(key_up: bool) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_F20,
                    wScan: 0,
                    dwFlags: if key_up {
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
    }

    #[test]
    #[ignore = "needs an interactive desktop session; run: cargo test --test synthetic_input -- --ignored"]
    fn injected_f20_reaches_listener() {
        let listener = KeyboardListener::new().expect("failed to spawn keyboard listener");
        // Give the hook thread a moment to install the hooks; spawn() does
        // not currently wait for installation (known gap, see review notes).
        std::thread::sleep(Duration::from_millis(200));

        send_f20(false);
        assert!(
            saw_key_event(&listener, Key::F20, true, Duration::from_secs(2)),
            "did not observe injected F20 key-down"
        );

        send_f20(true);
        assert!(
            saw_key_event(&listener, Key::F20, false, Duration::from_secs(2)),
            "did not observe injected F20 key-up"
        );
    }
}
