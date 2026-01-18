//! macOS keyboard listener using CGEventTap

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapCallBack, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use crate::error::{Error, Result};
use crate::platform::state::{BlockingHotkeys, ListenerState};
use crate::types::KeyEvent;

use super::keycode::{flags_to_modifiers, keycode_to_key, keycode_to_modifier};
use super::permissions::check_accessibility;

/// Internal listener state returned to KeyboardListener
pub(crate) struct MacOSListenerState {
    pub event_receiver: Receiver<KeyEvent>,
    pub thread_handle: Option<JoinHandle<()>>,
    pub running: Arc<AtomicBool>,
    pub blocking_hotkeys: Option<BlockingHotkeys>,
}

/// Spawn a macOS keyboard listener using CGEventTap
pub(crate) fn spawn(blocking_hotkeys: Option<BlockingHotkeys>) -> Result<MacOSListenerState> {
    if !check_accessibility() {
        return Err(Error::AccessibilityNotGranted);
    }

    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(ListenerState::new(tx, blocking_hotkeys.clone())));
    let running = Arc::new(AtomicBool::new(true));

    // Channel to communicate event tap creation success/failure
    let (init_tx, init_rx) = mpsc::channel::<std::result::Result<(), String>>();

    let thread_state = Arc::clone(&state);
    let thread_running = Arc::clone(&running);

    let handle = thread::spawn(move || {
        run_event_tap(thread_state, thread_running, init_tx);
    });

    // Wait for the event tap to be created
    match init_rx.recv() {
        Ok(Ok(())) => {
            // Event tap created successfully
        }
        Ok(Err(msg)) => {
            return Err(Error::EventTapCreationFailed(msg));
        }
        Err(_) => {
            return Err(Error::EventTapCreationFailed(
                "Event tap thread terminated unexpectedly".to_string(),
            ));
        }
    }

    Ok(MacOSListenerState {
        event_receiver: rx,
        thread_handle: Some(handle),
        running,
        blocking_hotkeys,
    })
}

/// The callback function for the event tap
///
/// Returns NULL to block the event, or the event pointer to pass it through.
unsafe extern "C-unwind" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // Safety: user_info is our state pointer
    let state = &*(user_info as *const Mutex<ListenerState>);

    let cg_event = event.as_ref();
    let flags = CGEvent::flags(Some(cg_event));
    let modifiers = flags_to_modifiers(flags);

    let mut should_block = false;

    if let Ok(mut state) = state.lock() {
        match event_type {
            CGEventType::KeyDown => {
                let keycode =
                    CGEvent::integer_value_field(Some(cg_event), CGEventField::KeyboardEventKeycode)
                        as u16;

                let key = keycode_to_key(keycode);

                // Check if this should be blocked
                should_block = state.should_block(modifiers, key);

                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key,
                    is_key_down: true,
                    changed_modifier: None,
                });
            }
            CGEventType::KeyUp => {
                let keycode =
                    CGEvent::integer_value_field(Some(cg_event), CGEventField::KeyboardEventKeycode)
                        as u16;

                let key = keycode_to_key(keycode);

                // Block key up if we blocked key down (to be consistent)
                should_block = state.should_block(modifiers, key);

                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key,
                    is_key_down: false,
                    changed_modifier: None,
                });
            }
            CGEventType::FlagsChanged => {
                let keycode =
                    CGEvent::integer_value_field(Some(cg_event), CGEventField::KeyboardEventKeycode)
                        as u16;
                let changed_modifier = keycode_to_modifier(keycode);

                // Check if this is a lock key (e.g., Caps Lock) which comes through
                // as FlagsChanged but isn't a traditional modifier
                let lock_key = keycode_to_key(keycode);

                let prev_mods = state.current_modifiers;
                state.current_modifiers = modifiers;

                // Handle lock keys specially - they come through FlagsChanged
                // but don't change our tracked modifier state
                if let Some(key) = lock_key {
                    // For lock keys, we need to track press/release via the alpha lock flag
                    // or just emit both down and up on each press
                    let is_key_down = flags.contains(CGEventFlags::MaskAlphaShift);

                    should_block = state.should_block(modifiers, Some(key));

                    let _ = state.event_sender.send(KeyEvent {
                        modifiers,
                        key: Some(key),
                        is_key_down,
                        changed_modifier: None,
                    });
                } else if modifiers != prev_mods {
                    // Regular modifier key - only emit if modifiers actually changed
                    // Determine press vs release by checking which bits changed
                    let gained = modifiers.bits() & !prev_mods.bits();
                    // A key is down if we gained any modifier bits
                    let is_key_down = gained != 0;

                    // Check if this modifier-only combo should be blocked
                    if is_key_down {
                        should_block = state.should_block(modifiers, None);
                    }

                    let _ = state.event_sender.send(KeyEvent {
                        modifiers,
                        key: None,
                        is_key_down,
                        changed_modifier,
                    });
                }
            }
            _ => {}
        }
    }

    if should_block {
        // Block the event from reaching other applications
        std::ptr::null_mut()
    } else {
        // Pass the event through unchanged
        event.as_ptr()
    }
}

/// Run the event tap in a dedicated thread
fn run_event_tap(
    state: Arc<Mutex<ListenerState>>,
    running: Arc<AtomicBool>,
    init_tx: Sender<std::result::Result<(), String>>,
) {
    // Event types we want to monitor
    let event_mask: CGEventMask = (1 << CGEventType::KeyDown.0)
        | (1 << CGEventType::KeyUp.0)
        | (1 << CGEventType::FlagsChanged.0);

    // Store state in a raw pointer for the callback
    let state_ptr = Arc::into_raw(Arc::clone(&state)) as *mut c_void;

    let callback: CGEventTapCallBack = Some(event_tap_callback);

    // Use Default mode (not ListenOnly) to enable optional event blocking
    let tap: Option<CFRetained<CFMachPort>> = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            event_mask,
            callback,
            state_ptr,
        )
    };

    let tap = match tap {
        Some(t) => t,
        None => {
            // Cleanup
            unsafe {
                let _ = Arc::from_raw(state_ptr as *const Mutex<ListenerState>);
            }
            let _ = init_tx.send(Err(
                "Failed to create event tap. Your terminal app may need accessibility permission in System Settings > Privacy & Security > Accessibility".to_string()
            ));
            return;
        }
    };

    // Create run loop source
    let source: Option<CFRetained<CFRunLoopSource>> =
        CFMachPort::new_run_loop_source(None, Some(&tap), 0);

    let source = match source {
        Some(s) => s,
        None => {
            unsafe {
                CFMachPort::invalidate(&tap);
                let _ = Arc::from_raw(state_ptr as *const Mutex<ListenerState>);
            }
            let _ = init_tx.send(Err("Failed to create run loop source".to_string()));
            return;
        }
    };

    // Get the current run loop and add the source
    let run_loop = CFRunLoop::current();

    // Unwrap the Option<CFRetained<CFRunLoop>> - current() should always succeed on a valid thread
    let run_loop = match run_loop {
        Some(rl) => rl,
        None => {
            unsafe {
                CFMachPort::invalidate(&tap);
                let _ = Arc::from_raw(state_ptr as *const Mutex<ListenerState>);
            }
            let _ = init_tx.send(Err("Failed to get current run loop".to_string()));
            return;
        }
    };

    run_loop.add_source(Some(&source), unsafe {
        objc2_core_foundation::kCFRunLoopCommonModes
    });
    CGEvent::tap_enable(&tap, true);

    // Signal successful initialization
    let _ = init_tx.send(Ok(()));

    // Run the loop
    while running.load(std::sync::atomic::Ordering::SeqCst) {
        // Run for a short interval, then check if we should stop
        CFRunLoop::run_in_mode(
            unsafe { objc2_core_foundation::kCFRunLoopDefaultMode },
            0.1, // 100ms timeout
            true,
        );
    }

    // Cleanup
    run_loop.remove_source(Some(&source), unsafe {
        objc2_core_foundation::kCFRunLoopCommonModes
    });
    CGEvent::tap_enable(&tap, false);
    CFMachPort::invalidate(&tap);
    unsafe {
        let _ = Arc::from_raw(state_ptr as *const Mutex<ListenerState>);
    }
}
