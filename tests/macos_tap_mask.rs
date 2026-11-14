//! Guards against the macOS listener filtering mouse buttons.
//!
//! A Default (filtering) tap holds every event in its mask until the callback
//! returns. handy-keys never blocks mouse buttons, but its single filtering
//! tap listed them, and back/forward (buttons 4/5) stopped reaching apps
//! system-wide while it ran (cjpais/Handy#1758, handy-keys#29). Mouse buttons
//! must only reach the listener through a ListenOnly tap.
//!
//! This reads the taps this process installed with CGGetEventTapList, the
//! same check used to diagnose the bug. Requires accessibility permission
//! (skips itself otherwise).

#![cfg(target_os = "macos")]

use std::time::Duration;

use handy_keys::KeyboardListener;

/// `CGEventTapInformation` from <CoreGraphics/CGEventTypes.h>.
#[repr(C)]
#[derive(Clone, Copy)]
struct CGEventTapInformation {
    event_tap_id: u32,
    tap_point: u32,
    options: u32,
    events_of_interest: u64,
    tapping_process: i32,
    process_being_tapped: i32,
    enabled: bool,
    min_usec_latency: f32,
    avg_usec_latency: f32,
    max_usec_latency: f32,
}

/// `kCGEventTapOptionDefault`: the tap can modify or drop events.
const TAP_OPTION_DEFAULT: u32 = 0;
/// `kCGEventTapOptionListenOnly`.
const TAP_OPTION_LISTEN_ONLY: u32 = 1;

/// kCGEventLeftMouseDown/Up, kCGEventRightMouseDown/Up,
/// kCGEventOtherMouseDown/Up.
const MOUSE_BUTTON_MASK: u64 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 25) | (1 << 26);
/// kCGEventKeyDown/KeyUp, kCGEventFlagsChanged.
const KEYBOARD_MASK: u64 = (1 << 10) | (1 << 11) | (1 << 12);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetEventTapList(
        max_number_of_taps: u32,
        tap_list: *mut CGEventTapInformation,
        event_tap_count: *mut u32,
    ) -> i32;
}

fn own_taps() -> Vec<CGEventTapInformation> {
    let mut count = 0;
    let err = unsafe { CGGetEventTapList(0, std::ptr::null_mut(), &mut count) };
    assert_eq!(err, 0, "CGGetEventTapList (count) failed: {err}");

    let mut taps = vec![
        CGEventTapInformation {
            event_tap_id: 0,
            tap_point: 0,
            options: 0,
            events_of_interest: 0,
            tapping_process: 0,
            process_being_tapped: 0,
            enabled: false,
            min_usec_latency: 0.0,
            avg_usec_latency: 0.0,
            max_usec_latency: 0.0,
        };
        count as usize
    ];
    let err = unsafe { CGGetEventTapList(count, taps.as_mut_ptr(), &mut count) };
    assert_eq!(err, 0, "CGGetEventTapList (list) failed: {err}");
    taps.truncate(count as usize);

    let pid = std::process::id() as i32;
    taps.into_iter()
        .filter(|t| t.tapping_process == pid)
        .collect()
}

#[test]
fn mouse_buttons_are_never_in_a_filtering_tap() {
    let listener = match KeyboardListener::new() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("SKIPPED: cannot create listener ({e})");
            return;
        }
    };
    std::thread::sleep(Duration::from_millis(200));

    let taps = own_taps();
    for t in &taps {
        println!(
            "tap {}: options={} mask={:#x} enabled={}",
            t.event_tap_id, t.options, t.events_of_interest, t.enabled
        );
    }

    for t in taps.iter().filter(|t| t.options == TAP_OPTION_DEFAULT) {
        assert_eq!(
            t.events_of_interest & MOUSE_BUTTON_MASK,
            0,
            "filtering tap {} includes mouse buttons (mask {:#x}); apps would \
             wait on the callback for every click (cjpais/Handy#1758)",
            t.event_tap_id,
            t.events_of_interest
        );
    }

    // Keyboard hotkeys must still be blockable, and mouse hotkeys must still
    // be heard.
    assert!(
        taps.iter().any(|t| t.options == TAP_OPTION_DEFAULT
            && t.events_of_interest & KEYBOARD_MASK == KEYBOARD_MASK),
        "no filtering tap covers keyboard events"
    );
    assert!(
        taps.iter().any(|t| t.options == TAP_OPTION_LISTEN_ONLY
            && t.events_of_interest & MOUSE_BUTTON_MASK == MOUSE_BUTTON_MASK),
        "no listen-only tap covers mouse buttons"
    );

    drop(listener);
}
