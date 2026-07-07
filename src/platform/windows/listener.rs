//! Windows low-level keyboard hook implementation

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    MsgWaitForMultipleObjects, PeekMessageW, RegisterClassW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT,
    PM_REMOVE, QS_ALLINPUT, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

use crate::error::Result;
use crate::platform::state::BlockingHotkeys;
use crate::types::{Key, KeyEvent, Modifiers};

use super::keycode::{is_altgr_phantom_ctrl, map_key, vk_to_modifier};

const HOOK_LOOP_TIMEOUT_MS: u32 = 10;

// WTS session notification plumbing not exposed by the `windows` crate bindings.
const NOTIFY_FOR_THIS_SESSION: u32 = 0;
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
// WM_WTSSESSION_CHANGE wParam values (wtsapi32.h).
const WTS_CONSOLE_CONNECT: usize = 0x1;
const WTS_REMOTE_CONNECT: usize = 0x3;
const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;

/// The side-specific modifier keys we track, paired with their virtual-key codes.
const MODIFIER_KEYS: [(VIRTUAL_KEY, Modifiers); 8] = [
    (VK_LWIN, Modifiers::CMD_LEFT),
    (VK_RWIN, Modifiers::CMD_RIGHT),
    (VK_LSHIFT, Modifiers::SHIFT_LEFT),
    (VK_RSHIFT, Modifiers::SHIFT_RIGHT),
    (VK_LCONTROL, Modifiers::CTRL_LEFT),
    (VK_RCONTROL, Modifiers::CTRL_RIGHT),
    (VK_LMENU, Modifiers::OPT_LEFT),
    (VK_RMENU, Modifiers::OPT_RIGHT),
];

/// Every modifier bit reconciliation may touch. FN is excluded: Windows never
/// reports it, so reconciliation must not clear it.
const RECONCILABLE: Modifiers = Modifiers::CMD
    .union(Modifiers::SHIFT)
    .union(Modifiers::CTRL)
    .union(Modifiers::OPT);

/// Thread-local state for the keyboard hook callback.
///
/// Windows low-level hooks require a callback function with a specific signature,
/// so we use thread-local storage to access our state from within the callback.
struct HookContext {
    event_sender: Sender<KeyEvent>,
    current_modifiers: Modifiers,
    blocking_hotkeys: Option<BlockingHotkeys>,
    /// AltGr's phantom Left Ctrl is currently held. The hook drops the
    /// phantom's own events, but GetAsyncKeyState still reports LCtrl as
    /// down while AltGr is held, so reconciliation must not adopt it.
    altgr_phantom_ctrl: bool,
}

thread_local! {
    static HOOK_CONTEXT: std::cell::RefCell<Option<HookContext>> = const { std::cell::RefCell::new(None) };
}

/// What draining the thread message queue observed.
#[derive(Default)]
struct DrainOutcome {
    /// WM_QUIT received -- exit the message loop.
    quit: bool,
    /// A session change (lock/unlock/connect) occurred -- reconcile modifier
    /// state, since key-ups on the secure desktop never reach the hook.
    session_change: bool,
    /// The interactive desktop came back (unlock or console/remote connect) --
    /// re-install hooks in case Windows silently removed them.
    reinstall_hooks: bool,
}

/// Drain all pending thread messages.
fn drain_thread_messages(msg: &mut MSG) -> DrainOutcome {
    let mut outcome = DrainOutcome::default();
    unsafe {
        while PeekMessageW(msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                outcome.quit = true;
                return outcome;
            }
            if msg.message == WM_WTSSESSION_CHANGE {
                match msg.wParam.0 {
                    WTS_SESSION_LOCK => outcome.session_change = true,
                    WTS_SESSION_UNLOCK | WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT => {
                        outcome.session_change = true;
                        outcome.reinstall_hooks = true;
                    }
                    _ => {}
                }
            }
            let _ = TranslateMessage(msg);
            DispatchMessageW(msg);
        }
    }
    outcome
}

/// Wait for new input/messages or until timeout expires.
fn wait_for_message_or_timeout(timeout_ms: u32) {
    unsafe {
        let _ = MsgWaitForMultipleObjects(None, false, timeout_ms, QS_ALLINPUT);
    }
}

/// Snapshot which tracked modifier keys are physically held right now.
///
/// Note: if the calling thread's desktop is not active (e.g. the lock screen's
/// secure desktop is up), GetAsyncKeyState reports every key as up -- which is
/// the correct answer for our purposes: treat everything as released.
fn physical_modifiers() -> Modifiers {
    let mut held = Modifiers::empty();
    for (vk, modifier) in MODIFIER_KEYS {
        // High bit set = key currently down.
        if unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0 {
            held |= modifier;
        }
    }
    held
}

/// Modifiers we track as held that are no longer physically held.
fn stale_modifiers(tracked: Modifiers, physical: Modifiers) -> Modifiers {
    (tracked & RECONCILABLE) & !physical
}

/// Physically-held modifiers we missed the press of and should adopt.
///
/// While AltGr's phantom Left Ctrl is held, CTRL_LEFT is excluded: it is
/// physically down per GetAsyncKeyState, but the user only pressed Right
/// Alt. A real Left Ctrl press during that window still sets CTRL_LEFT
/// through its own hook event (which carries a non-phantom scancode).
fn adoptable_modifiers(
    tracked: Modifiers,
    physical: Modifiers,
    altgr_phantom_ctrl: bool,
) -> Modifiers {
    let mut adopt = physical & RECONCILABLE & !tracked;
    if altgr_phantom_ctrl {
        adopt &= !Modifiers::CTRL_LEFT;
    }
    adopt
}

/// Build the synthetic release events that clear `stale` from `tracked`, in
/// MODIFIER_KEYS order. Each event carries the modifier set as it shrinks,
/// exactly as if the keys had been released one by one.
fn release_events(tracked: Modifiers, stale: Modifiers) -> Vec<KeyEvent> {
    let mut modifiers = tracked;
    let mut events = Vec::new();
    for (_, modifier) in MODIFIER_KEYS {
        if stale.contains(modifier) {
            modifiers &= !modifier;
            events.push(KeyEvent {
                modifiers,
                key: None,
                is_key_down: false,
                changed_modifier: Some(modifier),
            });
        }
    }
    events
}

/// Reconcile tracked modifiers against the physical keyboard state.
///
/// Corrects drift from missed events: secure desktop transitions swallow
/// key-ups (Win+L delivers the Win key DOWN but its UP happens on the secure
/// desktop), leaving modifiers stuck on until the user happens to press them
/// again. Mirrors `reconcile_modifiers` in the macOS listener.
///
/// Stale modifiers are cleared with synthetic release events so consumers
/// tracking press/release state recover; missed presses are adopted silently
/// and ride along on the next real event.
///
/// Known micro-race: GetAsyncKeyState reads the state *now*, not at the time
/// the event being processed was generated, so a modifier pressed in the
/// sub-millisecond window between a keystroke entering the queue and its hook
/// callback running gets adopted onto that earlier keystroke's event. It is
/// self-correcting (the modifier's own event arrives right after) and
/// unavoidable with this API — macOS avoids it because CGEvent flags are
/// stamped per event.
fn reconcile_modifiers(ctx: &mut HookContext) {
    let physical = physical_modifiers();
    // Phantom Ctrl only exists while AltGr is held; if LCtrl reads as up,
    // the phantom is gone (covers a phantom key-up swallowed by a secure
    // desktop transition, which would otherwise suppress CTRL_LEFT adoption
    // forever).
    if ctx.altgr_phantom_ctrl && !physical.contains(Modifiers::CTRL_LEFT) {
        ctx.altgr_phantom_ctrl = false;
    }
    for event in release_events(
        ctx.current_modifiers,
        stale_modifiers(ctx.current_modifiers, physical),
    ) {
        ctx.current_modifiers = event.modifiers;
        let _ = ctx.event_sender.send(event);
    }
    ctx.current_modifiers |=
        adoptable_modifiers(ctx.current_modifiers, physical, ctx.altgr_phantom_ctrl);
}

/// Reconcile modifier state from the hook thread's message loop (used on
/// session change, where no input event accompanies the state change).
fn reconcile_modifiers_in_context() {
    HOOK_CONTEXT.with(|ctx_cell| {
        if let Some(ctx) = ctx_cell.borrow_mut().as_mut() {
            reconcile_modifiers(ctx);
        }
    });
}

/// Wndproc for the session notification window. (The `windows` crate's
/// DefWindowProcW is a generic Rust wrapper, so it cannot be used as
/// lpfnWndProc directly.)
///
/// Reconciles here as well as in the drain loop: the drain loop covers posted
/// delivery of WM_WTSSESSION_CHANGE (what Windows 11 26200 does, verified
/// live), while this path covers builds that deliver it via SendMessage,
/// which bypasses the message queue. Double reconciliation on the posted path
/// is harmless — the second pass finds nothing stale. Hook reinstall stays
/// loop-driven; it is defensive hardening, while the state reset is the fix.
unsafe extern "system" fn session_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_WTSSESSION_CHANGE {
        match wparam.0 {
            WTS_SESSION_LOCK | WTS_SESSION_UNLOCK | WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT => {
                reconcile_modifiers_in_context();
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Create a message-only window registered for WTS session notifications.
/// WM_WTSSESSION_CHANGE is posted to it and picked out of the queue by
/// `drain_thread_messages` (verified live: lock 0x7 and unlock 0x8 both arrive).
///
/// Returns None on failure. Non-fatal: hooks still work, and stale modifiers
/// are still corrected lazily by `reconcile_modifiers` on the next key event.
unsafe fn create_session_notification_window() -> Option<HWND> {
    let class_name: Vec<u16> = "HandyKeysSessionWatcher\0".encode_utf16().collect();
    let wnd_class = WNDCLASSW {
        lpfnWndProc: Some(session_wndproc),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // May fail with ERROR_CLASS_ALREADY_EXISTS (e.g. a second listener in the
    // same process); CreateWindowExW below still succeeds against the
    // existing class, so the result is deliberately ignored.
    RegisterClassW(&wnd_class);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        None,
        None,
        None,
    )
    .ok()?;

    if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_err() {
        let _ = DestroyWindow(hwnd);
        return None;
    }

    Some(hwnd)
}

/// Clean up the session notification window. Must run on the creating thread.
unsafe fn destroy_session_notification_window(hwnd: HWND) {
    let _ = WTSUnRegisterSessionNotification(hwnd);
    let _ = DestroyWindow(hwnd);
}

/// Re-install the low-level hooks, defensively: Windows silently removes an LL
/// hook whose callback exceeds its timeout budget, and a session away from the
/// interactive desktop is a common moment for that to surface. (Note lock/unlock
/// does NOT inherently invalidate hooks -- they survived it in live testing.)
///
/// The replacements are installed before the old hooks are removed, so a
/// failure never leaves us hook-less. No messages are pumped between install
/// and unhook, so no event is delivered twice.
unsafe fn reinstall_hooks(kb_hook: &mut HHOOK, mouse_hook: &mut HHOOK) -> bool {
    let new_kb = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) {
        Ok(h) => h,
        Err(_) => return false,
    };
    let new_mouse = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) {
        Ok(h) => h,
        Err(_) => {
            let _ = UnhookWindowsHookEx(new_kb);
            return false;
        }
    };
    let _ = UnhookWindowsHookEx(*kb_hook);
    let _ = UnhookWindowsHookEx(*mouse_hook);
    *kb_hook = new_kb;
    *mouse_hook = new_mouse;
    true
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
                altgr_phantom_ctrl: false,
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
                // Clean up keyboard hook before returning
                unsafe {
                    let _ = UnhookWindowsHookEx(kb_hook);
                }
                return;
            }
        };

        // Watch for session changes (Win+L lock/unlock, RDP connect): the
        // secure desktop swallows key-up events, so modifier state must be
        // reconciled when the session comes back.
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
            let outcome = drain_thread_messages(&mut msg);
            if outcome.quit {
                break;
            }
            if outcome.session_change {
                reconcile_modifiers_in_context();
            }
            if outcome.reinstall_hooks {
                unsafe {
                    if !reinstall_hooks(&mut kb_hook, &mut mouse_hook) {
                        // Keep the old hooks: they usually still work (the
                        // reinstall is defensive hardening, not a repair).
                        eprintln!("handy-keys: failed to re-install hooks after session change");
                    }
                }
            }

            // Wait for messages or timeout — unlike thread::sleep, this returns
            // immediately when a message arrives, so hook callbacks are never delayed.
            wait_for_message_or_timeout(HOOK_LOOP_TIMEOUT_MS);
        }

        // Clean up the session notification window, then the hooks
        if let Some(hwnd) = session_hwnd {
            unsafe {
                destroy_session_notification_window(hwnd);
            }
        }
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
            let scan_code = kb_struct.scanCode;
            let is_extended = (kb_struct.flags.0 & LLKHF_EXTENDED.0) != 0;

            let is_key_down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);

            // On AltGr layouts Windows synthesizes a Left Ctrl press/release
            // around every Right Alt event (captured live on UK layout:
            // identical timestamps, scancode 0x21D). Drop it entirely — no
            // modifier update, no emit — so AltGr+key reads as OptRight
            // only. The event is still passed down the hook chain.
            if is_altgr_phantom_ctrl(vk_code, scan_code) {
                ctx.altgr_phantom_ctrl = is_key_down;
                return;
            }

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
            } else {
                // Non-modifier key: reconcile tracked modifiers against the
                // physical keyboard first, so a key-up missed during a secure
                // desktop transition can't stick a modifier onto this event.
                // (Not done for modifier events: inside a low-level hook the
                // async key state does not yet include the in-flight change,
                // so reconciling there would fight the toggle logic above.)
                reconcile_modifiers(ctx);

                if let Some(key) = map_key(vk_code, scan_code, is_extended) {
                    // Regular key event
                    should_block = should_block_hotkey(
                        &ctx.blocking_hotkeys,
                        ctx.current_modifiers,
                        Some(key),
                    );

                    let _ = ctx.event_sender.send(KeyEvent {
                        modifiers: ctx.current_modifiers,
                        key: Some(key),
                        is_key_down,
                        changed_modifier: None,
                    });
                }
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

    // Only button transitions matter; bail out early for moves and wheel
    // events so the hot path stays free of state access.
    if !matches!(
        wparam.0 as u32,
        WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
    ) {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let mut should_block = false;

    // Process the mouse event
    HOOK_CONTEXT.with(|ctx_cell| {
        let mut ctx_ref = ctx_cell.borrow_mut();
        if let Some(ctx) = ctx_ref.as_mut() {
            let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);

            // Stale modifiers must not gate button reporting (or decorate the
            // event), so reconcile before reading them.
            reconcile_modifiers(ctx);

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
                should_block =
                    should_block_hotkey(&ctx.blocking_hotkeys, ctx.current_modifiers, Some(key));

                let _ = ctx.event_sender.send(KeyEvent {
                    modifiers: ctx.current_modifiers,
                    key: Some(key),
                    is_key_down: is_down,
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
        assert!(drain_thread_messages(&mut msg).quit);
        clear_message_queue();
    }

    #[test]
    fn should_block_registered_mouse_hotkey() {
        use crate::types::Hotkey;
        let mut hotkeys = std::collections::HashSet::new();
        hotkeys.insert(Hotkey::new(Modifiers::empty(), Key::MouseX1).unwrap());
        let blocking_hotkeys = Some(std::sync::Arc::new(std::sync::Mutex::new(hotkeys)));

        assert!(should_block_hotkey(
            &blocking_hotkeys,
            Modifiers::empty(),
            Some(Key::MouseX1)
        ));
        assert!(!should_block_hotkey(
            &blocking_hotkeys,
            Modifiers::empty(),
            Some(Key::MouseX2)
        ));
    }

    fn post_session_change(wparam: usize) {
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
        unsafe {
            PostThreadMessageW(
                GetCurrentThreadId(),
                WM_WTSSESSION_CHANGE,
                WPARAM(wparam),
                LPARAM(0),
            )
            .unwrap();
        }
    }

    #[test]
    fn drain_reports_session_unlock_with_reinstall() {
        clear_message_queue();
        post_session_change(WTS_SESSION_UNLOCK);
        let mut msg = MSG::default();
        let outcome = drain_thread_messages(&mut msg);
        assert!(outcome.session_change);
        assert!(outcome.reinstall_hooks);
        assert!(!outcome.quit);
        clear_message_queue();
    }

    #[test]
    fn drain_reports_session_lock_without_reinstall() {
        clear_message_queue();
        post_session_change(WTS_SESSION_LOCK);
        let mut msg = MSG::default();
        let outcome = drain_thread_messages(&mut msg);
        assert!(outcome.session_change);
        assert!(!outcome.reinstall_hooks);
        clear_message_queue();
    }

    #[test]
    fn drain_ignores_unrelated_session_events() {
        clear_message_queue();
        // WTS_SESSION_LOGOFF (0x6) is not a state we react to.
        post_session_change(0x6);
        let mut msg = MSG::default();
        let outcome = drain_thread_messages(&mut msg);
        assert!(!outcome.session_change);
        assert!(!outcome.reinstall_hooks);
        clear_message_queue();
    }

    #[test]
    fn stale_modifiers_flags_released_keys() {
        // Tracked Win+Ctrl, but only Ctrl still physically held: Win is stale.
        let stale = stale_modifiers(
            Modifiers::CMD_LEFT | Modifiers::CTRL_LEFT,
            Modifiers::CTRL_LEFT,
        );
        assert_eq!(stale, Modifiers::CMD_LEFT);
    }

    #[test]
    fn stale_modifiers_empty_when_state_matches() {
        let tracked = Modifiers::SHIFT_LEFT | Modifiers::OPT_RIGHT;
        assert_eq!(stale_modifiers(tracked, tracked), Modifiers::empty());
        assert_eq!(
            stale_modifiers(Modifiers::empty(), Modifiers::CTRL_LEFT),
            Modifiers::empty()
        );
    }

    #[test]
    fn stale_modifiers_never_touches_fn() {
        // FN is not reconcilable on Windows: never reported stale.
        let stale = stale_modifiers(Modifiers::FN | Modifiers::CMD_LEFT, Modifiers::empty());
        assert_eq!(stale, Modifiers::CMD_LEFT);
    }

    #[test]
    fn release_events_shrink_modifiers_one_key_at_a_time() {
        let tracked = Modifiers::CMD_LEFT | Modifiers::CTRL_LEFT | Modifiers::FN;
        let stale = Modifiers::CMD_LEFT | Modifiers::CTRL_LEFT;
        let events = release_events(tracked, stale);

        assert_eq!(events.len(), 2);
        // MODIFIER_KEYS order: CMD_LEFT before CTRL_LEFT.
        assert_eq!(events[0].changed_modifier, Some(Modifiers::CMD_LEFT));
        assert_eq!(events[0].modifiers, Modifiers::CTRL_LEFT | Modifiers::FN);
        assert!(!events[0].is_key_down);
        assert_eq!(events[0].key, None);
        assert_eq!(events[1].changed_modifier, Some(Modifiers::CTRL_LEFT));
        assert_eq!(events[1].modifiers, Modifiers::FN);
        assert!(!events[1].is_key_down);
    }

    #[test]
    fn release_events_empty_when_nothing_stale() {
        assert!(release_events(Modifiers::CMD_LEFT, Modifiers::empty()).is_empty());
    }

    #[test]
    fn adoption_picks_up_missed_presses() {
        let adopt = adoptable_modifiers(
            Modifiers::CTRL_LEFT,
            Modifiers::CTRL_LEFT | Modifiers::SHIFT_LEFT,
            false,
        );
        assert_eq!(adopt, Modifiers::SHIFT_LEFT);
    }

    #[test]
    fn adoption_skips_ctrl_left_while_altgr_phantom_held() {
        // AltGr held: GetAsyncKeyState reports LCtrl+RAlt down, but only
        // OptRight reflects a user keypress. CTRL_LEFT must not be adopted.
        let adopt = adoptable_modifiers(
            Modifiers::OPT_RIGHT,
            Modifiers::CTRL_LEFT | Modifiers::OPT_RIGHT,
            true,
        );
        assert_eq!(adopt, Modifiers::empty());
    }

    #[test]
    fn adoption_keeps_other_modifiers_while_altgr_phantom_held() {
        // A genuinely-held Shift missed during a secure desktop transition
        // is still adopted while the phantom flag is set.
        let adopt = adoptable_modifiers(
            Modifiers::OPT_RIGHT,
            Modifiers::CTRL_LEFT | Modifiers::OPT_RIGHT | Modifiers::SHIFT_LEFT,
            true,
        );
        assert_eq!(adopt, Modifiers::SHIFT_LEFT);
    }

    #[test]
    fn phantom_flag_never_releases_a_tracked_ctrl() {
        // User really holds LCtrl, then AltGr: CTRL_LEFT is tracked from its
        // own event and physically down, so it must be neither stale nor
        // re-adopted.
        let tracked = Modifiers::CTRL_LEFT | Modifiers::OPT_RIGHT;
        let physical = Modifiers::CTRL_LEFT | Modifiers::OPT_RIGHT;
        assert_eq!(stale_modifiers(tracked, physical), Modifiers::empty());
        assert_eq!(
            adoptable_modifiers(tracked, physical, true),
            Modifiers::empty()
        );
    }
}
