//! Dev-time probe: what does macOS put in CGEventFlags for FlagsChanged?
//!
//! Installs a listen-only event tap (pass --active for a Default one),
//! posts modifier events in several shapes, and dumps the raw flags each
//! one arrives with — specifically whether the NX_DEVICE* left/right bits
//! (low 16 bits) are populated. The listener's modifier press/release
//! derivation (keycode.rs: modifier_is_key_down) is built on what this
//! probe observes.
//!
//! Findings on macOS 15 that the listener relies on:
//! - HID-posted (and hardware) modifier events arrive with per-side device
//!   bits computed by the system; caller-set flags pass through untouched,
//!   with no device bits added (phases 1-4).
//! - Releasing one of two held same-group keys can clear the side-agnostic
//!   group mask while the sibling's device bit stays set (phase 2, third
//!   event): the mask and device bits disagree, and the device bits are
//!   the truthful side.
//! - CGEventSourceFlagsState also reports device bits (phase 6), so
//!   reconciliation after a tap timeout can pick the correct side.
//!
//! Run: cargo run --example device_bits_probe
//! Needs accessibility permission. Posts brief Shift presses system-wide;
//! don't type while it runs (~2s).

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;
use std::time::Duration;

use objc2_core_foundation::{CFMachPort, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

static PHASE: AtomicU8 = AtomicU8::new(0);

const VK_LSHIFT: u16 = 0x38;
const VK_RSHIFT: u16 = 0x3C;

unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    let cg_event = unsafe { event.as_ref() };
    let flags = CGEvent::flags(Some(cg_event));
    let keycode =
        CGEvent::integer_value_field(Some(cg_event), CGEventField::KeyboardEventKeycode) as u16;
    let type_name = match event_type {
        CGEventType::KeyDown => "KeyDown",
        CGEventType::KeyUp => "KeyUp",
        CGEventType::FlagsChanged => "FlagsChanged",
        _ => "other",
    };
    println!(
        "[phase {}] {:<13} keycode={:#04x} flags={:#018x} device_bits={:#06x}",
        PHASE.load(Ordering::Relaxed),
        type_name,
        keycode,
        flags.0,
        flags.0 & 0xFFFF,
    );
    event.as_ptr()
}

fn post_key(source: &CGEventSource, keycode: u16, down: bool) {
    let event = CGEvent::new_keyboard_event(Some(source), keycode, down).unwrap();
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}

fn post_flags_changed(
    source: &CGEventSource,
    keycode: u16,
    down: bool,
    flags: CGEventFlags,
    location: CGEventTapLocation,
) {
    let event = CGEvent::new_keyboard_event(Some(source), keycode, down).unwrap();
    CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
    CGEvent::set_flags(Some(&event), flags);
    CGEvent::post(location, Some(&event));
}

fn settle() {
    thread::sleep(Duration::from_millis(200));
}

fn main() {
    thread::spawn(|| {
        let event_mask: CGEventMask = (1 << CGEventType::KeyDown.0)
            | (1 << CGEventType::KeyUp.0)
            | (1 << CGEventType::FlagsChanged.0);
        let options = if std::env::args().any(|a| a == "--active") {
            CGEventTapOptions::Default
        } else {
            CGEventTapOptions::ListenOnly
        };
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                options,
                event_mask,
                Some(callback),
                std::ptr::null_mut(),
            )
        }
        .expect("tap_create failed — grant accessibility permission");
        let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0).unwrap();
        let run_loop = CFRunLoop::current().unwrap();
        run_loop.add_source(Some(&source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
        CGEvent::tap_enable(&tap, true);
        CFRunLoop::run();
    });
    thread::sleep(Duration::from_millis(300));

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();

    println!("--- phase 1: plain KeyDown/KeyUp of RShift posted to HID tap ---");
    PHASE.store(1, Ordering::Relaxed);
    post_key(&source, VK_RSHIFT, true);
    thread::sleep(Duration::from_millis(50));
    post_key(&source, VK_RSHIFT, false);
    settle();

    println!("--- phase 2: overlapping LShift+RShift via HID tap ---");
    PHASE.store(2, Ordering::Relaxed);
    post_key(&source, VK_LSHIFT, true);
    thread::sleep(Duration::from_millis(30));
    post_key(&source, VK_RSHIFT, true);
    thread::sleep(Duration::from_millis(30));
    post_key(&source, VK_LSHIFT, false);
    thread::sleep(Duration::from_millis(30));
    post_key(&source, VK_RSHIFT, false);
    settle();

    println!("--- phase 3: explicit FlagsChanged, MaskShift only (no device bits), HID tap ---");
    PHASE.store(3, Ordering::Relaxed);
    post_flags_changed(
        &source,
        VK_LSHIFT,
        true,
        CGEventFlags::MaskShift,
        CGEventTapLocation::HIDEventTap,
    );
    thread::sleep(Duration::from_millis(50));
    post_flags_changed(
        &source,
        VK_LSHIFT,
        false,
        CGEventFlags(0),
        CGEventTapLocation::HIDEventTap,
    );
    settle();

    println!(
        "--- phase 4: explicit FlagsChanged, MaskShift only (no device bits), Session tap ---"
    );
    PHASE.store(4, Ordering::Relaxed);
    post_flags_changed(
        &source,
        VK_LSHIFT,
        true,
        CGEventFlags::MaskShift,
        CGEventTapLocation::SessionEventTap,
    );
    thread::sleep(Duration::from_millis(50));
    post_flags_changed(
        &source,
        VK_LSHIFT,
        false,
        CGEventFlags(0),
        CGEventTapLocation::SessionEventTap,
    );
    settle();

    println!("--- phase 5: caller-flag LShift down, then plain HID LShift down/up ---");
    PHASE.store(5, Ordering::Relaxed);
    post_flags_changed(
        &source,
        VK_LSHIFT,
        true,
        CGEventFlags::MaskShift,
        CGEventTapLocation::HIDEventTap,
    );
    thread::sleep(Duration::from_millis(50));
    post_key(&source, VK_LSHIFT, true);
    thread::sleep(Duration::from_millis(50));
    post_key(&source, VK_LSHIFT, false);
    thread::sleep(Duration::from_millis(50));
    post_flags_changed(
        &source,
        VK_LSHIFT,
        false,
        CGEventFlags(0),
        CGEventTapLocation::HIDEventTap,
    );
    settle();

    println!("--- phase 6: CGEventSourceFlagsState while RShift held via HID tap ---");
    PHASE.store(6, Ordering::Relaxed);
    post_key(&source, VK_RSHIFT, true);
    thread::sleep(Duration::from_millis(50));
    let held = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState);
    println!("CombinedSessionState while held: {:#018x}", held.0);
    post_key(&source, VK_RSHIFT, false);
    settle();

    let combined = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState);
    println!("final CombinedSessionState flags: {:#018x}", combined.0);
    println!("done");
}
