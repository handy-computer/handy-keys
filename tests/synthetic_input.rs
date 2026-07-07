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
//! Injections must never type into or otherwise disturb the session the
//! tests run in: key-down/key-up pairs use F20 (exists in the `Key` enum on
//! both platforms, does nothing in terminals or the desktop environment);
//! keys that would type a character are injected as lone key-ups, which the
//! low-level hook still observes but which generate no character.

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
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEINPUT, MOUSEEVENTF_MIDDLEUP, MOUSE_EVENT_FLAGS,
        VIRTUAL_KEY, VK_F20,
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

    /// Inject a key-up by scancode only (wVk = 0, KEYEVENTF_SCANCODE):
    /// Windows translates the scancode to a VK using the active layout,
    /// exactly as a hardware key release would arrive at the hook. A lone
    /// key-up generates no character, so nothing is typed into the session.
    fn send_scancode_key_up(scan: u16) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        assert_eq!(sent, 1, "SendInput failed");
    }

    /// The grave and quote key *positions* must map to Grave and Quote no
    /// matter which VK the active layout assigns them (cjpais/Handy#1516:
    /// on UK, the apostrophe key sends VK_OEM_3 and used to report Grave).
    ///
    /// Windows translates the injected scancode through the active layout,
    /// so this test proves layout independence when run under any layout
    /// (verified live under both US and UK on the original dev machine).
    /// It does not switch layouts itself: programmatic switching would
    /// perturb the interactive session the test runs in and requires the
    /// other layout to be installed, so it asserts per-scancode instead.
    #[test]
    #[ignore = "needs an interactive desktop session; run: cargo test --test synthetic_input -- --ignored"]
    fn punctuation_scancodes_map_positionally() {
        const SC_GRAVE: u16 = 0x29; // left of 1 (US `~, UK `¬)
        const SC_QUOTE: u16 = 0x28; // right of ; (US '", UK '@)

        let listener = KeyboardListener::new().expect("failed to spawn keyboard listener");
        std::thread::sleep(Duration::from_millis(200));

        send_scancode_key_up(SC_GRAVE);
        assert!(
            saw_key_event(&listener, Key::Grave, false, Duration::from_secs(2)),
            "grave-position scancode 0x29 did not map to Key::Grave"
        );

        send_scancode_key_up(SC_QUOTE);
        assert!(
            saw_key_event(&listener, Key::Quote, false, Duration::from_secs(2)),
            "quote-position scancode 0x28 did not map to Key::Quote"
        );
    }

    /// Inject a lone mouse-button event by flag. Mirrors the keyboard
    /// lone-key-up trick: a button-up with no preceding button-down is still
    /// observed by the low-level hook but performs no action in the session.
    fn send_mouse_flag(flags: MOUSE_EVENT_FLAGS, mouse_data: u32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        assert_eq!(sent, 1, "SendInput failed");
    }

    /// The mouse hook path reports button transitions end-to-end. Middle and
    /// X buttons are reported unconditionally (left/right are gated on a held
    /// modifier), so a lone middle-button-up is the disturbance-free probe.
    #[test]
    #[ignore = "needs an interactive desktop session; run: cargo test --test synthetic_input -- --ignored"]
    fn injected_mouse_button_reaches_listener() {
        let listener = KeyboardListener::new().expect("failed to spawn keyboard listener");
        std::thread::sleep(Duration::from_millis(200));

        send_mouse_flag(MOUSEEVENTF_MIDDLEUP, 0);
        assert!(
            saw_key_event(&listener, Key::MouseMiddle, false, Duration::from_secs(2)),
            "did not observe injected middle-button-up as Key::MouseMiddle"
        );
    }
}
