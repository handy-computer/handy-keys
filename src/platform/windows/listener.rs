//! Windows low-level keyboard hook implementation

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use std::ptr;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, MsgWaitForMultipleObjects,
    PeekMessageW, RegisterClassW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PM_REMOVE, QS_ALLINPUT,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

use crate::error::Result;
use crate::platform::state::BlockingHotkeys;
use crate::types::{Hotkey, Key, KeyEvent, Modifiers};

use super::keycode::{vk_to_key, vk_to_modifier};

const HOOK_LOOP_TIMEOUT_MS: u32 = 10;

// WTS constants not in the windows crate's generated bindings
const NOTIFY_FOR_THIS_SESSION: u32 = 0;
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_SESSION_UNLOCK: usize = 0x8;

/// Thread-local state for the keyboard hook callback.
///
/// Windows low-level hooks require a callback function with a specific signature,
/// so we use thread-local storage to access our state from within the callback.
struct HookContext {
    event_sender: Sender<KeyEvent>,
    current_modifiers: Modifiers,
    blocking_hotkeys: Option<BlockingHotkeys>,
}

thread_local! {
    static HOOK_CONTEXT: std::cell::RefCell<Option<HookContext>> = const { std::cell::RefCell::new(None) };
}

/// Result of draining the thread message queue.
enum DrainResult {
    /// Normal -- all messages processed, loop continues.
    Continue,
    /// WM_QUIT received -- exit the message loop.
    Quit,
    /// Windows session was unlocked -- hooks need re-installation.
    SessionUnlock,
}

/// Drain all pending thread messages.
fn drain_thread_messages(msg: &mut MSG) -> DrainResult {
    let mut session_unlocked = false;
    unsafe {
        while PeekMessageW(msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return DrainResult::Quit;
            }
            if msg.message == WM_WTSSESSION_CHANGE
                && msg.wParam == WPARAM(WTS_SESSION_UNLOCK)
            {
                session_unlocked = true;
            }
            let _ = TranslateMessage(msg);
            DispatchMessageW(msg);
        }
    }
    if session_unlocked {
        DrainResult::SessionUnlock
    } else {
        DrainResult::Continue
    }
}

/// Wait for new input/messages or until timeout expires.
fn wait_for_message_or_timeout(timeout_ms: u32) {
    unsafe {
        let _ = MsgWaitForMultipleObjects(None, false, timeout_ms, QS_ALLINPUT);
    }
}

/// Create a message-only window and register for session change notifications.
/// Returns the window handle, or `None` if setup fails (non-fatal -- hooks still work,
/// they just won't auto-recover after session lock/unlock).
unsafe fn create_session_notification_window() -> Option<HWND> {
    let class_name: Vec<u16> = "HandyKeysSessionWatcher\0".encode_utf16().collect();
    let wnd_class = WNDCLASSW {
        lpfnWndProc: Some(windows::Win32::UI::WindowsAndMessaging::DefWindowProcW),
        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    RegisterClassW(&wnd_class);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::PCWSTR(class_name.as_ptr()),
        windows::core::PCWSTR(ptr::null()),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        None,
        None,
    );

    let hwnd = match hwnd {
        Ok(h) => h,
        Err(_) => return None,
    };

    if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_err() {
        let _ = DestroyWindow(hwnd);
        return None;
    }

    Some(hwnd)
}

/// Clean up the session notification window.
unsafe fn destroy_session_notification_window(hwnd: HWND) {
    let _ = WTSUnRegisterSessionNotification(hwnd);
    let _ = DestroyWindow(hwnd);
}

/// Re-install low-level hooks after session unlock invalidated them.
/// Returns the new hook handles, or `None` on failure.
unsafe fn reinstall_hooks(
    old_kb: windows::Win32::UI::WindowsAndMessaging::HHOOK,
    old_mouse: windows::Win32::UI::WindowsAndMessaging::HHOOK,
) -> Option<(
    windows::Win32::UI::WindowsAndMessaging::HHOOK,
    windows::Win32::UI::WindowsAndMessaging::HHOOK,
)> {
    let _ = UnhookWindowsHookEx(old_kb);
    let _ = UnhookWindowsHookEx(old_mouse);

    let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0).ok()?;
    match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) {
        Ok(mouse) => Some((kb, mouse)),
        Err(_) => {
            let _ = UnhookWindowsHookEx(kb);
            None
        }
    }
}

/// Internal listener state returned to KeyboardListener
pub(crate) struct WindowsListenerState {
    pub event_receiver: mpsc::Receiver<KeyEvent>,
    pub thread_handle: Option<JoinHandle<()>>,
    pub running: Arc<AtomicBool>,
    pub blocking_hotkeys: Option<BlockingHotkeys>,
}

/// Spawn a Windows low-level keyboard hook listener
pub(crate) fn spawn(blocking_hotkeys: Option<BlockingHotkeys>) -> Result<WindowsListenerState> {
    let (tx, rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let thread_running = Arc::clone(&running);
    let thread_blocking = blocking_hotkeys.clone();

    let handle = thread::spawn(move || {
        // Initialize thread-local hook context
        HOOK_CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = Some(HookContext {
                event_sender: tx,
                current_modifiers: Modifiers::empty(),
                blocking_hotkeys: thread_blocking,
            });
        });

        // Install the low-level keyboard hook
        let kb_hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) };

        let mut kb_hook = match kb_hook {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Failed to install keyboard hook: {:?}", e);
                return;
            }
        };

        // Install the low-level mouse hook
        let mouse_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) };

        let mut mouse_hook = match mouse_hook {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Failed to install mouse hook: {:?}", e);
                unsafe {
                    let _ = UnhookWindowsHookEx(kb_hook);
                }
                return;
            }
        };

        // Register for session change notifications so we can re-install hooks
        // after Win+L lock/unlock (the Winlogon desktop switch invalidates them).
        let session_hwnd = unsafe { create_session_notification_window() };

        // Message loop - required for low-level hooks to function.
        // Keep the short timeout so shutdown polling behavior remains unchanged.
        let mut msg = MSG::default();
        loop {
            // Check if we should stop
            if !thread_running.load(Ordering::SeqCst) {
                break;
            }

            // Process all pending messages
            match drain_thread_messages(&mut msg) {
                DrainResult::Quit => break,
                DrainResult::SessionUnlock => unsafe {
                    if let Some((new_kb, new_mouse)) = reinstall_hooks(kb_hook, mouse_hook) {
                        kb_hook = new_kb;
                        mouse_hook = new_mouse;
                        eprintln!("handy-keys: hooks re-installed after session unlock");
                    } else {
                        eprintln!("handy-keys: failed to re-install hooks after session unlock");
                        break;
                    }
                },
                DrainResult::Continue => {}
            }

            // Wait for messages or timeout -- unlike thread::sleep, this returns
            // immediately when a message arrives, so hook callbacks are never delayed.
            wait_for_message_or_timeout(HOOK_LOOP_TIMEOUT_MS);
        }

        // Clean up session notification window
        if let Some(hwnd) = session_hwnd {
            unsafe {
                destroy_session_notification_window(hwnd);
            }
        }

        // Clean up the hooks
        unsafe {
            let _ = UnhookWindowsHookEx(kb_hook);
            let _ = UnhookWindowsHookEx(mouse_hook);
        }

        // Clear thread-local state
        HOOK_CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = None;
        });
    });

    Ok(WindowsListenerState {
        event_receiver: rx,
        thread_handle: Some(handle),
        running,
        blocking_hotkeys,
    })
}

/// Low-level keyboard hook callback
///
/// This function is called by Windows for every keyboard event system-wide.
/// It must return quickly to avoid input lag.
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // If code < 0, we must pass to next hook without processing
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let mut should_block = false;

    // Process the keyboard event
    HOOK_CONTEXT.with(|ctx_cell| {
        let mut ctx_ref = ctx_cell.borrow_mut();
        if let Some(ctx) = ctx_ref.as_mut() {
            // Extract key information from KBDLLHOOKSTRUCT
            let kb_struct = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk_code = kb_struct.vkCode as u16;
            let is_extended = (kb_struct.flags.0 & LLKHF_EXTENDED.0) != 0;

            let is_key_down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);

            // Check if this is a modifier key
            if let Some(modifier) = vk_to_modifier(vk_code) {
                let prev_modifiers = ctx.current_modifiers;

                // Update modifier state
                if is_key_down {
                    ctx.current_modifiers |= modifier;
                } else {
                    ctx.current_modifiers &= !modifier;
                }

                // Only emit event if modifiers actually changed
                if ctx.current_modifiers != prev_modifiers {
                    // Check if modifier-only combo should be blocked
                    should_block =
                        should_block_hotkey(&ctx.blocking_hotkeys, ctx.current_modifiers, None);

                    let _ = ctx.event_sender.send(KeyEvent {
                        modifiers: ctx.current_modifiers,
                        key: None,
                        is_key_down,
                        changed_modifier: Some(modifier),
                    });
                }
            } else if let Some(key) = vk_to_key(vk_code, is_extended) {
                // Regular key event
                should_block =
                    should_block_hotkey(&ctx.blocking_hotkeys, ctx.current_modifiers, Some(key));

                let _ = ctx.event_sender.send(KeyEvent {
                    modifiers: ctx.current_modifiers,
                    key: Some(key),
                    is_key_down,
                    changed_modifier: None,
                });
            }
        }
    });

    if should_block {
        // Return non-zero to block the event from propagating
        LRESULT(1)
    } else {
        // Pass to next hook in chain
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Low-level mouse hook callback
///
/// This function is called by Windows for every mouse event system-wide.
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // If code < 0, we must pass to next hook without processing
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // Process the mouse event
    HOOK_CONTEXT.with(|ctx_cell| {
        let mut ctx_ref = ctx_cell.borrow_mut();
        if let Some(ctx) = ctx_ref.as_mut() {
            let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);

            // Only report left/right clicks when modifiers are held (to avoid noise)
            let has_modifiers = !ctx.current_modifiers.is_empty();

            let (key, is_down) = match wparam.0 as u32 {
                WM_LBUTTONDOWN if has_modifiers => (Some(Key::MouseLeft), true),
                WM_LBUTTONUP if has_modifiers => (Some(Key::MouseLeft), false),
                WM_RBUTTONDOWN if has_modifiers => (Some(Key::MouseRight), true),
                WM_RBUTTONUP if has_modifiers => (Some(Key::MouseRight), false),
                // Middle and X buttons always reported
                WM_MBUTTONDOWN => (Some(Key::MouseMiddle), true),
                WM_MBUTTONUP => (Some(Key::MouseMiddle), false),
                WM_XBUTTONDOWN => {
                    // High word of mouseData contains which X button (1 or 2)
                    let xbutton = (mouse_struct.mouseData >> 16) & 0xFFFF;
                    let key = if xbutton == 1 {
                        Some(Key::MouseX1)
                    } else if xbutton == 2 {
                        Some(Key::MouseX2)
                    } else {
                        None
                    };
                    (key, true)
                }
                WM_XBUTTONUP => {
                    let xbutton = (mouse_struct.mouseData >> 16) & 0xFFFF;
                    let key = if xbutton == 1 {
                        Some(Key::MouseX1)
                    } else if xbutton == 2 {
                        Some(Key::MouseX2)
                    } else {
                        None
                    };
                    (key, false)
                }
                _ => (None, false),
            };

            if let Some(key) = key {
                let _ = ctx.event_sender.send(KeyEvent {
                    modifiers: ctx.current_modifiers,
                    key: Some(key),
                    is_key_down: is_down,
                    changed_modifier: None,
                });
            }
        }
    });

    // Always pass mouse events through (no blocking for mouse)
    CallNextHookEx(None, code, wparam, lparam)
}

/// Check if a hotkey combination should be blocked
fn should_block_hotkey(
    blocking_hotkeys: &Option<BlockingHotkeys>,
    modifiers: Modifiers,
    key: Option<Key>,
) -> bool {
    if let Some(ref hotkeys) = blocking_hotkeys {
        if let Ok(set) = hotkeys.lock() {
            return set
                .iter()
                .any(|h| h.modifiers.matches(modifiers) && h.key == key);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::PostQuitMessage;

    fn clear_message_queue() {
        let mut msg = MSG::default();
        unsafe { while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {} }
    }

    #[test]
    fn wait_times_out_when_no_messages() {
        clear_message_queue();
        let start = Instant::now();
        wait_for_message_or_timeout(20);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(8),
            "expected wait to block close to timeout, elapsed={elapsed:?}"
        );
        clear_message_queue();
    }

    #[test]
    fn wait_returns_immediately_when_message_is_pending() {
        clear_message_queue();
        unsafe {
            PostQuitMessage(0);
        }
        let start = Instant::now();
        wait_for_message_or_timeout(200);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "expected pending message to wake wait early, elapsed={elapsed:?}"
        );
        clear_message_queue();
    }

    #[test]
    fn drain_messages_stops_on_wm_quit() {
        clear_message_queue();
        unsafe {
            PostQuitMessage(0);
        }
        let mut msg = MSG::default();
        assert!(matches!(drain_thread_messages(&mut msg), DrainResult::Quit));
        clear_message_queue();
    }
}
