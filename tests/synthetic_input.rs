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
//! Injections must never type into or otherwise disturb the session the
//! tests run in: key-down/key-up pairs use F20 (exists in the `Key` enum on
//! every platform, does nothing in terminals or the desktop environment);
//! keys that would type a character are injected as lone key-ups, which the
//! low-level hook still observes but which generate no character.

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

    fn count_event_nodes() -> usize {
        std::fs::read_dir("/dev/input")
            .expect("cannot read /dev/input")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
            .count()
    }

    /// Two blocking listeners must not grab each other's re-injection
    /// clones: doing so would mint new uinput devices in an unbounded
    /// loop. The second blocker degrades to detect-only behind the first.
    #[test]
    #[ignore = "needs /dev/input + /dev/uinput access; run: cargo test --test synthetic_input -- --ignored"]
    fn two_blocking_listeners_do_not_multiply_devices() {
        let _serial = SERIAL.lock().unwrap();
        let mut keyboard = virtual_keyboard();
        sleep(DEVICE_SETTLE);

        let first_hotkeys: BlockingHotkeys = Arc::new(Mutex::new(HashSet::from(["f20"
            .parse::<Hotkey>()
            .unwrap()])));
        let _first = KeyboardListener::new_with_blocking(first_hotkeys)
            .expect("first blocking listener failed to spawn");
        sleep(DEVICE_SETTLE);
        let baseline = count_event_nodes();

        let second_hotkeys: BlockingHotkeys = Arc::new(Mutex::new(HashSet::from(["f20"
            .parse::<Hotkey>()
            .unwrap()])));
        let _second = KeyboardListener::new_with_blocking(second_hotkeys)
            .expect("second blocking listener failed to spawn");
        sleep(DEVICE_SETTLE);

        // Stir the pipeline; a clone storm feeds on device activity too.
        emit_key(&mut keyboard, KeyCode::KEY_F20, true);
        emit_key(&mut keyboard, KeyCode::KEY_F20, false);

        let sample_one = count_event_nodes();
        sleep(Duration::from_millis(1500));
        let sample_two = count_event_nodes();

        assert_eq!(
            sample_one, sample_two,
            "device count still changing — clone storm between blocking listeners"
        );
        assert!(
            sample_two <= baseline + 1,
            "second blocking listener created clones it should not have \
             (baseline {baseline}, now {sample_two})"
        );
    }

    /// A keyboard with keys physically held at spawn must not be grabbed
    /// until it goes quiet: grabbing mid-press splits the press/release
    /// pair across two devices and wedges the key at the compositor.
    #[test]
    #[ignore = "needs /dev/input + /dev/uinput access; run: cargo test --test synthetic_input -- --ignored"]
    fn grab_is_deferred_while_keys_are_held() {
        use evdev::raw_stream::RawDevice;

        let _serial = SERIAL.lock().unwrap();
        let mut keyboard = virtual_keyboard();
        sleep(DEVICE_SETTLE);
        let node = keyboard
            .enumerate_dev_nodes_blocking()
            .expect("cannot enumerate virtual keyboard nodes")
            .find_map(|n| n.ok())
            .expect("virtual keyboard has no device node");

        // Hold F20 across the blocking listener's spawn.
        emit_key(&mut keyboard, KeyCode::KEY_F20, true);
        sleep(Duration::from_millis(200));

        let hotkeys: BlockingHotkeys = Arc::new(Mutex::new(HashSet::from(["f20"
            .parse::<Hotkey>()
            .unwrap()])));
        let blocker = KeyboardListener::new_with_blocking(hotkeys)
            .expect("blocking listener failed to spawn");
        sleep(Duration::from_millis(300));

        // While the key is held, the device must still be grabbable by us —
        // i.e. the listener deferred its grab.
        {
            let mut probe = RawDevice::open(&node).expect("cannot open virtual keyboard node");
            probe
                .grab()
                .expect("device was grabbed while a key was held — grab was not deferred");
            probe.ungrab().expect("probe ungrab failed");
        }

        // Release → device quiet → the listener finishes the grab.
        emit_key(&mut keyboard, KeyCode::KEY_F20, false);
        sleep(Duration::from_millis(500));
        {
            let mut probe = RawDevice::open(&node).expect("cannot open virtual keyboard node");
            assert!(
                probe.grab().is_err(),
                "device still not grabbed after all keys were released"
            );
        }

        // And the completed grab actually blocks: drain the events observed
        // so far, then verify a fresh press still reaches the listener.
        while blocker.try_recv().is_some() {}
        emit_key(&mut keyboard, KeyCode::KEY_F20, true);
        assert!(
            saw_key_event(&blocker, Key::F20, true, Duration::from_secs(2)),
            "listener stopped seeing events after the deferred grab completed"
        );
        emit_key(&mut keyboard, KeyCode::KEY_F20, false);
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
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_MIDDLEUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
        VIRTUAL_KEY, VK_F20, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK,
        VK_MEDIA_STOP,
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

    /// Inject a lone key-up for a virtual key. Media actions (play/pause,
    /// skip) fire on key-*down*, so a lone key-up is still observed by the
    /// hook without actually controlling any media in the running session.
    fn send_vk_key_up(vk: VIRTUAL_KEY) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        assert_eq!(sent, 1, "SendInput failed");
    }

    /// The four media virtual-keys map to their `Key` variants through the
    /// real WH_KEYBOARD_LL path. Only reaches the hook when the keyboard/driver
    /// delivers media keys as VK codes (not as WM_APPCOMMAND) — see TESTING.md.
    #[test]
    #[ignore = "needs an interactive desktop session; run: cargo test --test synthetic_input -- --ignored"]
    fn injected_media_keys_reach_listener() {
        let listener = KeyboardListener::new().expect("failed to spawn keyboard listener");
        std::thread::sleep(Duration::from_millis(200));

        for (vk, key) in [
            (VK_MEDIA_PLAY_PAUSE, Key::PlayPause),
            (VK_MEDIA_STOP, Key::Stop),
            (VK_MEDIA_PREV_TRACK, Key::PrevTrack),
            (VK_MEDIA_NEXT_TRACK, Key::NextTrack),
        ] {
            send_vk_key_up(vk);
            assert!(
                saw_key_event(&listener, key, false, Duration::from_secs(2)),
                "did not observe injected media key {key} (vk {:#04x})",
                vk.0
            );
        }
    }
}
