//! Platform integration tests driven by synthetic input injection.
//!
//! These tests exercise the real platform backend end-to-end: they spawn a
//! `KeyboardListener`, inject input through the OS (`CGEventPost` on macOS,
//! `SendInput` on Windows, a uinput virtual keyboard on Linux), and assert
//! on what comes out of the event channel.
//!
//! They are `#[ignore]`d because they need a real machine session (macOS:
//! accessibility permission; Windows: an interactive desktop; Linux: read
//! access to `/dev/input` and write access to `/dev/uinput`). Run them on
//! the target machine as part of the pre-merge checklist:
//!
//! ```sh
//! cargo test --test synthetic_input -- --ignored
//! ```
//!
//! All injections use F20: it exists in the `Key` enum on every platform
//! and does nothing in terminals or the desktop environment, so a test run
//! never types into or otherwise disturbs the session it runs in.

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

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::thread::sleep;

    use evdev::uinput::VirtualDevice;
    use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
    use handy_keys::{BlockingHotkeys, Hotkey, KeyEvent, Modifiers};

    /// Every test injects through a system-global uinput keyboard, so a
    /// listener spawned by one test would see another test's injections.
    /// Serialize them regardless of the harness's --test-threads setting.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// How long to wait after creating the virtual device before injecting:
    /// udev must grant the `input` group read access to the new node, and
    /// the listener must pick it up (startup scan or inotify hotplug).
    const DEVICE_SETTLE: Duration = Duration::from_millis(600);

    const IGNORE_REASON: &str = "needs /dev/input read access (input group) and /dev/uinput \
                                 write access; run: cargo test --test synthetic_input -- --ignored";

    fn virtual_keyboard() -> VirtualDevice {
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::KEY_F20);
        keys.insert(KeyCode::KEY_LEFTSHIFT);
        VirtualDevice::builder()
            .expect(
                "failed to open /dev/uinput — the synthetic-input tests need write access to it \
                 (e.g. sudo chmod 666 /dev/uinput for the session, or a udev rule granting it \
                 to your user)",
            )
            .name("handy-keys synthetic test keyboard")
            .with_keys(&keys)
            .expect("failed to declare virtual keyboard keys")
            .build()
            .expect("failed to create uinput virtual keyboard")
    }

    fn emit_key(keyboard: &mut VirtualDevice, code: KeyCode, pressed: bool) {
        // emit() appends the terminating SYN_REPORT itself.
        let event = InputEvent::new(EventType::KEY.0, code.0, i32::from(pressed));
        keyboard.emit(&[event]).expect("failed to inject key event");
    }

    /// Drain listener events until one satisfies `pred`, or time out.
    fn saw_event(
        listener: &KeyboardListener,
        timeout: Duration,
        pred: impl Fn(&KeyEvent) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match listener.recv_timeout(remaining) {
                Ok(event) if pred(&event) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }

    #[test]
    #[ignore = "needs /dev/input + /dev/uinput access; run: cargo test --test synthetic_input -- --ignored"]
    fn injected_f20_reaches_listener() {
        let _serial = SERIAL.lock().unwrap();
        // Device exists before the listener spawns: covers the startup scan.
        let mut keyboard = virtual_keyboard();
        sleep(DEVICE_SETTLE);

        let listener = KeyboardListener::new().expect(IGNORE_REASON);

        emit_key(&mut keyboard, KeyCode::KEY_F20, true);
        assert!(
            saw_key_event(&listener, Key::F20, true, Duration::from_secs(2)),
            "did not observe injected F20 key-down"
        );

        emit_key(&mut keyboard, KeyCode::KEY_F20, false);
        assert!(
            saw_key_event(&listener, Key::F20, false, Duration::from_secs(2)),
            "did not observe injected F20 key-up"
        );
    }

    #[test]
    #[ignore = "needs /dev/input + /dev/uinput access; run: cargo test --test synthetic_input -- --ignored"]
    fn hotplugged_keyboard_reaches_listener() {
        let _serial = SERIAL.lock().unwrap();
        // Listener spawns first, device appears afterwards: covers the
        // inotify hotplug path.
        let listener = KeyboardListener::new().expect(IGNORE_REASON);

        let mut keyboard = virtual_keyboard();
        sleep(DEVICE_SETTLE);

        emit_key(&mut keyboard, KeyCode::KEY_F20, true);
        assert!(
            saw_key_event(&listener, Key::F20, true, Duration::from_secs(2)),
            "did not observe F20 key-down from hotplugged keyboard"
        );

        emit_key(&mut keyboard, KeyCode::KEY_F20, false);
        assert!(
            saw_key_event(&listener, Key::F20, false, Duration::from_secs(2)),
            "did not observe F20 key-up from hotplugged keyboard"
        );
    }

    /// End-to-end blocking: a listener with a blocking hotkey grabs the
    /// virtual keyboard and re-injects through its uinput clone. A second,
    /// read-only listener can only see that clone (the grab starves its fd
    /// on the real device), so it observes exactly what the rest of the
    /// system would: the blocked F20 never appears, the unblocked Shift
    /// does.
    #[test]
    #[ignore = "needs /dev/input + /dev/uinput access; run: cargo test --test synthetic_input -- --ignored"]
    fn blocked_hotkey_is_withheld_from_other_consumers() {
        let _serial = SERIAL.lock().unwrap();
        let mut keyboard = virtual_keyboard();
        sleep(DEVICE_SETTLE);

        let hotkeys: BlockingHotkeys = Arc::new(Mutex::new(HashSet::from(["f20"
            .parse::<Hotkey>()
            .expect("f20 parses as a hotkey")])));
        let blocker = KeyboardListener::new_with_blocking(hotkeys).expect(
            "new_with_blocking failed — blocking additionally needs write access to /dev/uinput",
        );
        // Let udev finish setting up the blocker's uinput clone before the
        // observer scans for devices.
        sleep(DEVICE_SETTLE);
        let observer = KeyboardListener::new().expect(IGNORE_REASON);

        // Blocked key: the blocking listener reports it; the observer,
        // reading only the re-injected stream, must never see it.
        emit_key(&mut keyboard, KeyCode::KEY_F20, true);
        assert!(
            saw_key_event(&blocker, Key::F20, true, Duration::from_secs(2)),
            "blocking listener did not observe the F20 key-down it blocks"
        );
        assert!(
            !saw_key_event(&observer, Key::F20, true, Duration::from_secs(1)),
            "blocked F20 key-down leaked to another consumer"
        );
        emit_key(&mut keyboard, KeyCode::KEY_F20, false);
        assert!(
            saw_key_event(&blocker, Key::F20, false, Duration::from_secs(2)),
            "blocking listener did not observe the F20 key-up"
        );
        assert!(
            !saw_key_event(&observer, Key::F20, false, Duration::from_secs(1)),
            "blocked F20 key-up leaked to another consumer"
        );

        // Non-matching key: re-injected, so the observer sees it.
        emit_key(&mut keyboard, KeyCode::KEY_LEFTSHIFT, true);
        assert!(
            saw_event(&observer, Duration::from_secs(2), |e| {
                e.changed_modifier == Some(Modifiers::SHIFT_LEFT) && e.is_key_down
            }),
            "unblocked Shift press was not re-injected through the clone"
        );
        emit_key(&mut keyboard, KeyCode::KEY_LEFTSHIFT, false);
        assert!(
            saw_event(&observer, Duration::from_secs(2), |e| {
                e.changed_modifier == Some(Modifiers::SHIFT_LEFT) && !e.is_key_down
            }),
            "unblocked Shift release was not re-injected through the clone"
        );
    }

    #[test]
    #[ignore = "needs /dev/input + /dev/uinput access; run: cargo test --test synthetic_input -- --ignored"]
    fn modifiers_ride_on_injected_keys() {
        let _serial = SERIAL.lock().unwrap();
        let mut keyboard = virtual_keyboard();
        sleep(DEVICE_SETTLE);

        let listener = KeyboardListener::new().expect(IGNORE_REASON);

        emit_key(&mut keyboard, KeyCode::KEY_LEFTSHIFT, true);
        assert!(
            saw_event(&listener, Duration::from_secs(2), |e| {
                e.changed_modifier == Some(Modifiers::SHIFT_LEFT) && e.is_key_down
            }),
            "did not observe LeftShift modifier press"
        );

        emit_key(&mut keyboard, KeyCode::KEY_F20, true);
        assert!(
            saw_event(&listener, Duration::from_secs(2), |e| {
                e.key == Some(Key::F20)
                    && e.is_key_down
                    && e.modifiers.contains(Modifiers::SHIFT_LEFT)
            }),
            "F20 key-down did not carry the held Shift modifier"
        );

        emit_key(&mut keyboard, KeyCode::KEY_F20, false);
        emit_key(&mut keyboard, KeyCode::KEY_LEFTSHIFT, false);
        assert!(
            saw_event(&listener, Duration::from_secs(2), |e| {
                e.changed_modifier == Some(Modifiers::SHIFT_LEFT) && !e.is_key_down
            }),
            "did not observe LeftShift modifier release"
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
