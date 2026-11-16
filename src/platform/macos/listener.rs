//! macOS keyboard listener using CGEventTap

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFAbsoluteTimeGetCurrent, CFMachPort, CFRetained, CFRunLoop,
    CFRunLoopSource, CFRunLoopTimer, CFRunLoopTimerContext,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource, CGEventSourceStateID,
    CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};

use crate::error::{Error, Result};
use crate::platform::state::{BlockingHotkeys, ListenerState};
use crate::types::{Key, KeyEvent, Modifiers};

use super::keycode::{flags_have_alpha_shift, flags_have_fn, keycode_to_key, keycode_to_modifier};

/// The tap thread's run loop, handed back to the public listener so Drop can
/// stop it. CFRunLoop is one of the documented thread-safe CF types; objc2
/// doesn't mark it Send, hence the wrapper.
pub(crate) struct TapRunLoop(CFRetained<CFRunLoop>);

unsafe impl Send for TapRunLoop {}

impl TapRunLoop {
    /// Stop the tap thread's run loop. Callable from any thread.
    ///
    /// A bare CFRunLoopStop only affects a loop that is currently between
    /// entry and exit — the stop flag lives in per-run data that is reset on
    /// every entry — so a stop landing in the gap between the `running`
    /// check and CFRunLoopRun entering would be silently lost and the tap
    /// thread would park forever. Enqueue the stop as a run-loop block
    /// instead: the block queue persists across entries, so the request
    /// executes either right away (after the wake-up) or at the next entry.
    /// Both behaviors verified empirically on macOS 26.5: a pre-entry
    /// CFRunLoopStop is a no-op, a pre-entry enqueued block stops the loop
    /// the moment it is entered.
    pub(crate) fn stop(&self) {
        let block = block2::StackBlock::new(|| {
            if let Some(rl) = CFRunLoop::current() {
                rl.stop();
            }
        });
        unsafe {
            self.0.perform_block(
                objc2_core_foundation::kCFRunLoopCommonModes.map(|m| m.as_ref()),
                Some(&block),
            );
        }
        self.0.wake_up();
    }
}

/// Internal listener state returned to KeyboardListener
pub(crate) struct MacOSListenerState {
    pub event_receiver: Receiver<KeyEvent>,
    pub thread_handle: Option<JoinHandle<()>>,
    pub running: Arc<AtomicBool>,
    pub blocking_hotkeys: Option<BlockingHotkeys>,
    pub run_loop: TapRunLoop,
    /// Set by the tap thread when it stopped because the accessibility
    /// permission was revoked.
    pub accessibility_revoked: Arc<AtomicBool>,
}

/// Spawn a macOS keyboard listener using CGEventTap
pub(crate) fn spawn(blocking_hotkeys: Option<BlockingHotkeys>) -> Result<MacOSListenerState> {
    // Ask WindowServer rather than AXIsProcessTrusted(), whose cached answer
    // can be stale in either direction (see may_run_active_tap).
    if !may_run_active_tap() {
        return Err(Error::AccessibilityNotGranted);
    }

    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(ListenerState::new(tx, blocking_hotkeys.clone())));
    let running = Arc::new(AtomicBool::new(true));
    let accessibility_revoked = Arc::new(AtomicBool::new(false));

    // Channel reporting event tap creation: the tap thread's run loop on
    // success (needed to stop it later), or an error message.
    let (init_tx, init_rx) = mpsc::channel::<std::result::Result<TapRunLoop, String>>();

    let thread_state = Arc::clone(&state);
    let thread_running = Arc::clone(&running);
    let thread_revoked = Arc::clone(&accessibility_revoked);

    let handle = thread::spawn(move || {
        run_event_tap(
            thread_state,
            thread_running,
            thread_revoked,
            may_run_active_tap,
            init_tx,
        );
    });

    // Wait for the event tap to be created
    let run_loop = match init_rx.recv() {
        Ok(Ok(run_loop)) => run_loop,
        Ok(Err(msg)) => {
            return Err(Error::EventTapCreationFailed(msg));
        }
        Err(_) => {
            return Err(Error::EventTapCreationFailed(
                "Event tap thread terminated unexpectedly".to_string(),
            ));
        }
    };

    Ok(MacOSListenerState {
        event_receiver: rx,
        thread_handle: Some(handle),
        running,
        blocking_hotkeys,
        run_loop,
        accessibility_revoked,
    })
}

/// Reconcile internally tracked modifiers against the actual CGEventFlags from the OS.
///
/// This corrects drift caused by missed events (e.g., tap disabled by timeout, system
/// interruptions like Mission Control or screen lock). Should only be called for
/// non-FlagsChanged events, where flags reflect the current state with no change pending.
fn reconcile_modifiers(current: &mut Modifiers, flags: CGEventFlags) {
    // If OS says a modifier group is NOT held, clear our tracked bits.
    // This fixes "stuck modifier" from missed release events.
    if !flags.contains(CGEventFlags::MaskControl) {
        current.remove(Modifiers::CTRL_LEFT | Modifiers::CTRL_RIGHT);
    }
    if !flags.contains(CGEventFlags::MaskShift) {
        current.remove(Modifiers::SHIFT_LEFT | Modifiers::SHIFT_RIGHT);
    }
    if !flags.contains(CGEventFlags::MaskCommand) {
        current.remove(Modifiers::CMD_LEFT | Modifiers::CMD_RIGHT);
    }
    if !flags.contains(CGEventFlags::MaskAlternate) {
        current.remove(Modifiers::OPT_LEFT | Modifiers::OPT_RIGHT);
    }

    // If OS says a modifier group IS held but we have no bits for it,
    // we missed a press event. Default to left side as fallback.
    if flags.contains(CGEventFlags::MaskControl) && !current.intersects(Modifiers::CTRL) {
        current.insert(Modifiers::CTRL_LEFT);
    }
    if flags.contains(CGEventFlags::MaskShift) && !current.intersects(Modifiers::SHIFT) {
        current.insert(Modifiers::SHIFT_LEFT);
    }
    if flags.contains(CGEventFlags::MaskCommand) && !current.intersects(Modifiers::CMD) {
        current.insert(Modifiers::CMD_LEFT);
    }
    if flags.contains(CGEventFlags::MaskAlternate) && !current.intersects(Modifiers::OPT) {
        current.insert(Modifiers::OPT_LEFT);
    }
}

/// Which part of a key press a KeyDown/KeyUp event is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PressPhase {
    /// The initial key-down.
    Down,
    /// An autorepeat key-down.
    Repeat,
    /// The key-up.
    Up,
}

/// Blocking decisions per key press. A key-down blocked as a hotkey keeps its
/// autorepeats and key-up blocked, whatever the modifiers are by then. A
/// key-up always follows its key-down's decision.
///
/// The repeats of a press whose key-down passed are still judged on their own
/// modifiers: pressing Space just before Opt and holding fires Opt+Space from
/// the repeats, and those repeats must not also type into the app.
///
/// Judging every event on its own modifiers splits a press between us and the
/// focused app as soon as the modifiers change mid-press: releasing Cmd before
/// Space leaked an orphan Space key-up, and a modifier dropped while the key
/// was held let its autorepeats through, typing spaces while the hotkey was
/// still pressed (cjpais/Handy#2228). The reverse also happened: a press that
/// passed could have its key-up blocked, leaving the key stuck down in the app.
///
/// A press whose key-down we never saw (the listener started mid-press, or the
/// tap was disabled) has no decision; its events are judged on their own
/// modifiers, exactly as before.
#[derive(Debug, Default)]
struct PressBlocking {
    /// Keycode of each press in progress -> whether it is blocked.
    decided: HashMap<u16, bool>,
}

impl PressBlocking {
    /// Whether to block this event. `matches_hotkey`: whether the event, judged
    /// on its own modifiers, matches a blocking hotkey.
    fn decide(&mut self, keycode: u16, phase: PressPhase, matches_hotkey: bool) -> bool {
        match phase {
            PressPhase::Down => {
                // A fresh press always decides anew, which also replaces a
                // stale decision left by a key-up we missed.
                self.decided.insert(keycode, matches_hotkey);
                matches_hotkey
            }
            // A blocked press stays blocked. The repeats of a press that
            // passed (or that we never saw) are judged on their own, as
            // before: the manager fires the hotkey on a matching repeat, so
            // letting it through would reach the app as well.
            PressPhase::Repeat => self.decided.get(&keycode) == Some(&true) || matches_hotkey,
            PressPhase::Up => self.decided.remove(&keycode).unwrap_or(matches_hotkey),
        }
    }

    /// Forget every press in progress. Used when the tap was disabled: key
    /// events may have been missed, so recorded decisions can be stale.
    fn forget_all(&mut self) {
        self.decided.clear();
    }
}

/// Whether an event was posted by this process (e.g. text typed or pasted
/// with enigo by the app that owns this listener).
fn posted_by_this_process(event: &CGEvent) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID)
        == i64::from(std::process::id())
}

/// Context handed to the event tap callback through the refcon pointer.
///
/// `tap` is filled in right after `CGEventTapCreate` returns. The callback and
/// `run_event_tap` execute on the same thread (callbacks only fire while that
/// thread runs its run loop), so a plain `Cell`/`RefCell` is sound.
struct CallbackCtx {
    state: Arc<Mutex<ListenerState>>,
    tap: Cell<Option<NonNull<CFMachPort>>>,
    presses: RefCell<PressBlocking>,
    /// Whether the accessibility permission is still there.
    access: RefCell<AccessWatch>,
    /// One-shot re-check of the permission, armed by `check_access`. None in
    /// unit tests that drive the callback without a run loop.
    recheck_timer: Cell<Option<NonNull<CFRunLoopTimer>>>,
    /// Set when the listener stops because the permission was revoked.
    accessibility_revoked: Arc<AtomicBool>,
    /// The permission check (may_run_active_tap); replaceable in tests.
    is_trusted: fn() -> bool,
    /// We switched the tap off ourselves (check_access). macOS answers that
    /// with a TapDisabledByUserInput pseudo-event, which must not count as
    /// another permission check.
    paused_by_us: Cell<bool>,
}

/// How long to wait before checking the accessibility permission again.
/// A single "not trusted" answer around a change in System Settings could be
/// a transient one, so it only pauses the tap until a second check confirms
/// it.
const ACCESS_RECHECK_DELAY_SECS: f64 = 0.5;

/// Interval of the re-check timer. CFRunLoopTimer only lets a repeating timer
/// be re-armed after it fires, so it "repeats" this far apart, which never
/// happens in practice: it only fires when `check_access` arms it.
const RECHECK_TIMER_IDLE_SECS: f64 = 100.0 * 365.0 * 24.0 * 3600.0;

/// Revoking the accessibility permission while the tap runs leaves an active
/// tap at the head of the session that no longer gets events, and every
/// keyboard and mouse event of the session waits on it: the whole Mac stops
/// responding until the process exits. This tracks the permission so the tap
/// gets out of the way instead.
#[derive(Debug, Default)]
struct AccessWatch {
    /// "Not trusted" answers in a row.
    strikes: u8,
}

/// What to do with the tap after checking the permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessAction {
    /// Trusted: the tap should be on.
    Enable,
    /// Not trusted (once): take the tap out of the input path and check again
    /// shortly, since the answer may be stale.
    PauseAndRecheck,
    /// Not trusted twice in a row: stop the listener.
    Stop,
}

impl AccessWatch {
    fn observe(&mut self, trusted: bool) -> AccessAction {
        if trusted {
            self.strikes = 0;
            AccessAction::Enable
        } else {
            self.strikes = self.strikes.saturating_add(1);
            if self.strikes >= 2 {
                AccessAction::Stop
            } else {
                AccessAction::PauseAndRecheck
            }
        }
    }
}

impl CallbackCtx {
    fn new(
        state: Arc<Mutex<ListenerState>>,
        accessibility_revoked: Arc<AtomicBool>,
        is_trusted: fn() -> bool,
    ) -> Self {
        CallbackCtx {
            state,
            tap: Cell::new(None),
            presses: RefCell::new(PressBlocking::default()),
            access: RefCell::new(AccessWatch::default()),
            recheck_timer: Cell::new(None),
            accessibility_revoked,
            is_trusted,
            paused_by_us: Cell::new(false),
        }
    }

    fn set_tap_enabled(&self, enabled: bool) {
        self.paused_by_us.set(!enabled);
        if let Some(tap) = self.tap.get() {
            // Safety: the tap outlives the context (see run_event_tap).
            CGEvent::tap_enable(unsafe { tap.as_ref() }, enabled);
        }
    }

    /// Arm the re-check timer to fire after ACCESS_RECHECK_DELAY_SECS.
    fn schedule_access_recheck(&self) {
        if let Some(timer) = self.recheck_timer.get() {
            // Safety: the timer outlives the context (see run_event_tap).
            unsafe { timer.as_ref() }
                .set_next_fire_date(CFAbsoluteTimeGetCurrent() + ACCESS_RECHECK_DELAY_SECS);
        }
    }

    /// Check the accessibility permission and act on it. Runs on the tap
    /// thread only. `enable_if_trusted`: switch the tap on when trusted (it
    /// may have been disabled by macOS or by an earlier check).
    fn check_access(&self, enable_if_trusted: bool) -> AccessAction {
        let trusted = (self.is_trusted)();
        let action = self.access.borrow_mut().observe(trusted);
        match action {
            AccessAction::Enable => {
                if enable_if_trusted {
                    self.set_tap_enabled(true);
                }
            }
            AccessAction::PauseAndRecheck => {
                self.set_tap_enabled(false);
                self.schedule_access_recheck();
            }
            AccessAction::Stop => {
                self.set_tap_enabled(false);
                self.accessibility_revoked.store(true, Ordering::SeqCst);
                // run_event_tap sees the flag once the loop returns and tears
                // the tap down. A no-op when no loop is running (unit tests).
                if let Some(rl) = CFRunLoop::current() {
                    rl.stop();
                }
            }
        }
        action
    }

    /// The accessibility list changed: check now, and once more shortly in
    /// case the change had not reached WindowServer yet.
    fn on_access_changed(&self) {
        // A trusted answer leaves the tap as it is, unless an earlier check
        // paused it.
        let paused = self.access.borrow().strikes > 0;
        if self.check_access(paused) == AccessAction::Enable {
            self.schedule_access_recheck();
        }
    }
}

/// Re-check timer callback (tap thread).
unsafe extern "C-unwind" fn access_recheck_fired(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    // Safety: info is the CallbackCtx owned by run_event_tap, which
    // invalidates this timer before freeing it.
    let ctx = &*(info as *const CallbackCtx);
    let paused = ctx.access.borrow().strikes > 0;
    ctx.check_access(paused);
}

// ---- Accessibility change notifications ----
//
// tccd posts the Darwin notification "com.apple.tcc.access.changed" after a
// privacy permission changed, e.g. a switch in Privacy & Security >
// Accessibility. It arrives through libdispatch, so it works whether or not
// the app's main thread runs a run loop.
//
// Not used on purpose: the distributed notification
// "com.apple.accessibility.api". CF delivers it only on the main thread's
// run loop, and AXIsProcessTrusted() called right after it caches the old
// answer for good (verified on macOS 27).

/// tccd's "a permission changed" Darwin notification.
const TCC_ACCESS_CHANGED: &std::ffi::CStr = c"com.apple.tcc.access.changed";

extern "C" {
    fn notify_register_dispatch(
        name: *const std::ffi::c_char,
        out_token: *mut i32,
        queue: *mut c_void,
        handler: &block2::DynBlock<dyn Fn(i32)>,
    ) -> u32;
    fn notify_cancel(token: i32) -> u32;
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
}

thread_local! {
    /// The CallbackCtx of the tap running on this thread, for blocks queued
    /// onto its run loop.
    static TAP_CTX: Cell<*const CallbackCtx> = const { Cell::new(std::ptr::null()) };
}

impl TapRunLoop {
    /// Have the tap thread re-check the accessibility permission. Callable
    /// from any thread. Does nothing once the tap thread is gone.
    fn notify_access_changed(&self) {
        let block = block2::StackBlock::new(|| {
            TAP_CTX.with(|ctx| {
                let ctx = ctx.get();
                if !ctx.is_null() {
                    // Safety: TAP_CTX is cleared before the context is freed.
                    unsafe { &*ctx }.on_access_changed();
                }
            });
        });
        unsafe {
            self.0
                .perform_block(kCFRunLoopCommonModes.map(|m| m.as_ref()), Some(&block));
        }
        self.0.wake_up();
    }
}

/// Subscribe this tap thread to permission changes. Returns the token for
/// `unobserve_access_changes`.
fn observe_access_changes(run_loop: &CFRetained<CFRunLoop>) -> Option<i32> {
    // The handler keeps its own reference to the run loop: a notification
    // racing the listener's shutdown queues a block that never runs (the
    // thread has left its run loop), and TAP_CTX guards the rest.
    let run_loop = TapRunLoop(run_loop.clone());
    let handler = block2::RcBlock::new(move |_token: i32| {
        run_loop.notify_access_changed();
    });
    let mut token = 0;
    let status = unsafe {
        notify_register_dispatch(
            TCC_ACCESS_CHANGED.as_ptr(),
            &mut token,
            dispatch_get_global_queue(0, 0),
            &handler,
        )
    };
    (status == 0).then_some(token)
}

fn unobserve_access_changes(token: i32) {
    unsafe { notify_cancel(token) };
}

/// Whether we may run an active event tap, asked fresh. AXIsProcessTrusted()
/// answers from a per-process cache that only a main-thread run loop
/// refreshes (and that a call at the wrong moment pins to a stale value), so
/// it can keep saying "trusted" long after the permission was revoked. Creating
/// an active tap makes WindowServer check the permission itself. The probe
/// tap is never enabled or added to a run loop, so it sees no events.
fn may_run_active_tap() -> bool {
    unsafe extern "C-unwind" fn pass(
        _proxy: CGEventTapProxy,
        _event_type: CGEventType,
        event: NonNull<CGEvent>,
        _user_info: *mut c_void,
    ) -> *mut CGEvent {
        event.as_ptr()
    }
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::TailAppendEventTap,
            CGEventTapOptions::Default,
            1 << CGEventType::Null.0,
            Some(pass),
            std::ptr::null_mut(),
        )
    };
    match tap {
        Some(tap) => {
            CFMachPort::invalidate(&tap);
            true
        }
        None => false,
    }
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
    // Safety: user_info is the CallbackCtx owned by run_event_tap
    let ctx = &*(user_info as *const CallbackCtx);

    // Handle tap-disable pseudo-events before touching the state lock: macOS
    // stops delivering real events until the tap is re-enabled, so recovery
    // must not depend on the mutex being healthy. Re-enabling from the
    // callback (rather than polling CGEventTapIsEnabled from the run loop) is
    // the Apple-documented pattern and avoids recurring WindowServer RPCs —
    // those leak kernel IPC vouchers and eventually panic the whole machine
    // (cjpais/Handy#1827).
    //
    // Only re-enable while we still have the accessibility permission: once
    // it is revoked, re-enabling would put a tap that gets no events back in
    // front of all input, freezing the Mac. check_access leaves the tap off
    // then and stops the listener once the revocation is confirmed.
    if matches!(
        event_type,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        // Our own tap_enable(false) comes back as TapDisabledByUserInput;
        // the re-check timer already decides what happens next.
        if !(event_type == CGEventType::TapDisabledByUserInput && ctx.paused_by_us.get()) {
            ctx.check_access(true);
        }
        // Key events may have been missed while disabled; forget per-press
        // decisions so they cannot apply to a later, unrelated press.
        ctx.presses.borrow_mut().forget_all();
        // Reconcile modifiers: we may have missed events while disabled.
        if let Ok(mut state) = ctx.state.lock() {
            reconcile_modifiers(
                &mut state.current_modifiers,
                CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState),
            );
        }
        return event.as_ptr();
    }

    let cg_event = event.as_ref();
    let flags = CGEvent::flags(Some(cg_event));

    let mut should_block = false;

    if let Ok(mut state) = ctx.state.lock() {
        // Reconcile tracked modifiers against OS flags for non-FlagsChanged events.
        // On FlagsChanged, flags reflect the state *after* the current change, so
        // reconciling would fight with the toggle logic.
        let mut event_modifiers = state.current_modifiers;
        if event_type != CGEventType::FlagsChanged {
            reconcile_modifiers(&mut event_modifiers, flags);

            // Our own posted events (e.g. enigo typing with flags forced to
            // zero) don't reflect what the user is holding; storing them would
            // invert the FlagsChanged toggle below. Still report them with
            // their own flags.
            if !posted_by_this_process(cg_event) {
                state.current_modifiers = event_modifiers;
            }
        }

        // Build side-specific modifiers from internally tracked state + FN from flags
        let modifiers = if flags_have_fn(flags) {
            event_modifiers | Modifiers::FN
        } else {
            event_modifiers & !Modifiers::FN
        };

        match event_type {
            CGEventType::KeyDown => {
                let keycode = CGEvent::integer_value_field(
                    Some(cg_event),
                    CGEventField::KeyboardEventKeycode,
                ) as u16;

                let key = keycode_to_key(keycode);

                // Skip keycodes we can't map (e.g. special function keys like
                // Mission Control's 0xA0, or the Globe key's configured action).
                // A key-less KeyEvent carries no usable information, and the
                // Windows/Linux backends already emit mapped keys only.
                if key.is_none() {
                    return event.as_ptr();
                }

                let phase = if CGEvent::integer_value_field(
                    Some(cg_event),
                    CGEventField::KeyboardEventAutorepeat,
                ) != 0
                {
                    PressPhase::Repeat
                } else {
                    PressPhase::Down
                };
                should_block = ctx.presses.borrow_mut().decide(
                    keycode,
                    phase,
                    state.should_block(modifiers, key),
                );

                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key,
                    is_key_down: true,
                    changed_modifier: None,
                });
            }
            CGEventType::KeyUp => {
                let keycode = CGEvent::integer_value_field(
                    Some(cg_event),
                    CGEventField::KeyboardEventKeycode,
                ) as u16;

                let key = keycode_to_key(keycode);

                // Skip unmapped keycodes (same as KeyDown)
                if key.is_none() {
                    return event.as_ptr();
                }

                // Block the key-up iff its key-down was blocked
                should_block = ctx.presses.borrow_mut().decide(
                    keycode,
                    PressPhase::Up,
                    state.should_block(modifiers, key),
                );

                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key,
                    is_key_down: false,
                    changed_modifier: None,
                });
            }
            CGEventType::FlagsChanged => {
                let keycode = CGEvent::integer_value_field(
                    Some(cg_event),
                    CGEventField::KeyboardEventKeycode,
                ) as u16;

                let changed_modifier = keycode_to_modifier(keycode);

                // Check if this is a lock key (e.g., Caps Lock) which comes through
                // as FlagsChanged but isn't a traditional modifier
                let lock_key = keycode_to_key(keycode);

                // Handle lock keys specially - they come through FlagsChanged
                // but don't change our tracked modifier state
                if let Some(key) = lock_key {
                    let is_key_down = flags_have_alpha_shift(flags);

                    should_block = state.should_block(modifiers, Some(key));

                    let _ = state.event_sender.send(KeyEvent {
                        modifiers,
                        key: Some(key),
                        is_key_down,
                        changed_modifier: None,
                    });
                } else if let Some(modifier_bit) = changed_modifier {
                    // Regular modifier key — use keycode to toggle the specific bit
                    let was_set = state.current_modifiers.contains(modifier_bit);
                    let is_key_down = !was_set;

                    if is_key_down {
                        state.current_modifiers |= modifier_bit;
                    } else {
                        state.current_modifiers &= !modifier_bit;
                    }

                    // Re-derive modifiers after update (include FN from flags)
                    let new_modifiers = if flags_have_fn(flags) {
                        state.current_modifiers | Modifiers::FN
                    } else {
                        state.current_modifiers & !Modifiers::FN
                    };

                    // Check if this modifier-only combo should be blocked
                    if is_key_down {
                        should_block = state.should_block(new_modifiers, None);
                    }

                    let _ = state.event_sender.send(KeyEvent {
                        modifiers: new_modifiers,
                        key: None,
                        is_key_down,
                        changed_modifier,
                    });
                } else if keycode == 0x3F {
                    // FN key itself — tracked via flags, not keycode state
                    let had_fn = modifiers.contains(Modifiers::FN);
                    let has_fn = flags_have_fn(flags);
                    if had_fn != has_fn {
                        let new_modifiers = if has_fn {
                            state.current_modifiers | Modifiers::FN
                        } else {
                            state.current_modifiers & !Modifiers::FN
                        };

                        if has_fn {
                            should_block = state.should_block(new_modifiers, None);
                        }

                        let _ = state.event_sender.send(KeyEvent {
                            modifiers: new_modifiers,
                            key: None,
                            is_key_down: has_fn,
                            changed_modifier: Some(Modifiers::FN),
                        });
                    }
                }
            }
            // Mouse button events
            // Only report left/right clicks when modifiers are held (to avoid noise)
            CGEventType::LeftMouseDown if !modifiers.is_empty() => {
                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key: Some(Key::MouseLeft),
                    is_key_down: true,
                    changed_modifier: None,
                });
            }
            CGEventType::LeftMouseUp if !modifiers.is_empty() => {
                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key: Some(Key::MouseLeft),
                    is_key_down: false,
                    changed_modifier: None,
                });
            }
            CGEventType::RightMouseDown if !modifiers.is_empty() => {
                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key: Some(Key::MouseRight),
                    is_key_down: true,
                    changed_modifier: None,
                });
            }
            CGEventType::RightMouseUp if !modifiers.is_empty() => {
                let _ = state.event_sender.send(KeyEvent {
                    modifiers,
                    key: Some(Key::MouseRight),
                    is_key_down: false,
                    changed_modifier: None,
                });
            }
            // Pass through unmodified left/right clicks
            CGEventType::LeftMouseDown
            | CGEventType::LeftMouseUp
            | CGEventType::RightMouseDown
            | CGEventType::RightMouseUp => {}
            CGEventType::OtherMouseDown => {
                let button_number = CGEvent::integer_value_field(
                    Some(cg_event),
                    CGEventField::MouseEventButtonNumber,
                );
                let key = match button_number {
                    2 => Some(Key::MouseMiddle),
                    3 => Some(Key::MouseX1),
                    4 => Some(Key::MouseX2),
                    _ => None, // Unknown button
                };
                if let Some(key) = key {
                    let _ = state.event_sender.send(KeyEvent {
                        modifiers,
                        key: Some(key),
                        is_key_down: true,
                        changed_modifier: None,
                    });
                }
            }
            CGEventType::OtherMouseUp => {
                let button_number = CGEvent::integer_value_field(
                    Some(cg_event),
                    CGEventField::MouseEventButtonNumber,
                );
                let key = match button_number {
                    2 => Some(Key::MouseMiddle),
                    3 => Some(Key::MouseX1),
                    4 => Some(Key::MouseX2),
                    _ => None,
                };
                if let Some(key) = key {
                    let _ = state.event_sender.send(KeyEvent {
                        modifiers,
                        key: Some(key),
                        is_key_down: false,
                        changed_modifier: None,
                    });
                }
            }
            // TapDisabledByTimeout/TapDisabledByUserInput are handled (and
            // returned from) before the state lock above.
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
    accessibility_revoked: Arc<AtomicBool>,
    is_trusted: fn() -> bool,
    init_tx: Sender<std::result::Result<TapRunLoop, String>>,
) {
    // Event types we want to monitor
    let event_mask: CGEventMask = (1 << CGEventType::KeyDown.0)
        | (1 << CGEventType::KeyUp.0)
        | (1 << CGEventType::FlagsChanged.0)
        // Mouse buttons
        | (1 << CGEventType::LeftMouseDown.0)
        | (1 << CGEventType::LeftMouseUp.0)
        | (1 << CGEventType::RightMouseDown.0)
        | (1 << CGEventType::RightMouseUp.0)
        | (1 << CGEventType::OtherMouseDown.0)
        | (1 << CGEventType::OtherMouseUp.0);

    // Callback context, handed to the tap as a raw refcon pointer
    let ctx_ptr = Box::into_raw(Box::new(CallbackCtx::new(
        Arc::clone(&state),
        accessibility_revoked,
        is_trusted,
    )));

    let callback: CGEventTapCallBack = Some(event_tap_callback);

    // Use Default mode (not ListenOnly) to enable optional event blocking
    let tap: Option<CFRetained<CFMachPort>> = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            event_mask,
            callback,
            ctx_ptr as *mut c_void,
        )
    };

    let tap = match tap {
        Some(t) => t,
        None => {
            // Cleanup
            unsafe {
                drop(Box::from_raw(ctx_ptr));
            }
            let _ = init_tx.send(Err(
                "Failed to create event tap. Your terminal app may need accessibility permission in System Settings > Privacy & Security > Accessibility".to_string()
            ));
            return;
        }
    };

    // Give the callback the tap port so it can re-enable on TapDisabledBy*
    // pseudo-events. No race: callbacks only run on this thread's run loop,
    // which hasn't started yet.
    unsafe { (*ctx_ptr).tap.set(Some(NonNull::from(&*tap))) };

    // Create run loop source
    let source: Option<CFRetained<CFRunLoopSource>> =
        CFMachPort::new_run_loop_source(None, Some(&tap), 0);

    let source = match source {
        Some(s) => s,
        None => {
            unsafe {
                CFMachPort::invalidate(&tap);
                drop(Box::from_raw(ctx_ptr));
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
                drop(Box::from_raw(ctx_ptr));
            }
            let _ = init_tx.send(Err("Failed to get current run loop".to_string()));
            return;
        }
    };

    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });

    // Permission re-check timer: idle until check_access arms it.
    let mut timer_ctx = CFRunLoopTimerContext {
        version: 0,
        info: ctx_ptr as *mut c_void,
        retain: None,
        release: None,
        copyDescription: None,
    };
    let recheck_timer = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + RECHECK_TIMER_IDLE_SECS,
            RECHECK_TIMER_IDLE_SECS,
            0,
            0,
            Some(access_recheck_fired),
            &mut timer_ctx,
        )
    };
    if let Some(timer) = &recheck_timer {
        run_loop.add_timer(Some(timer), unsafe { kCFRunLoopCommonModes });
        unsafe { (*ctx_ptr).recheck_timer.set(Some(NonNull::from(&**timer))) };
    }

    // Hear about permission changes (see observe_access_changes).
    TAP_CTX.with(|c| c.set(ctx_ptr));
    let observer = observe_access_changes(&run_loop);

    CGEvent::tap_enable(&tap, true);

    // Signal successful initialization, handing back this thread's run loop
    // so KeyboardListener::drop can stop it.
    let _ = init_tx.send(Ok(TapRunLoop(run_loop.clone())));

    // Park in the run loop: no periodic wakeups, no watchdog. v0.3.2 cycled
    // CFRunLoopRunInMode every 100ms and polled CGEventTapIsEnabled each
    // pass; that RPC leaked kernel IPC vouchers until macOS panicked with
    // "Cannot grow ipc space beyond IVAC_ENTRIES_MAX" (cjpais/Handy#1827),
    // and even the bare 100ms cycling cost ~10 mach messages/sec. Tap
    // recovery is event-driven in event_tap_callback; shutdown goes through
    // TapRunLoop::stop, or check_access when the accessibility permission
    // was revoked. Guarded by `running` in case the loop returns without a
    // stop request; bail out if the tap's port died so we don't spin on a
    // dead source.
    let ctx = unsafe { &*ctx_ptr };
    while running.load(Ordering::SeqCst) && !ctx.accessibility_revoked.load(Ordering::SeqCst) {
        CFRunLoop::run();

        if !CFMachPort::is_valid(&tap) {
            break;
        }
    }

    // Cleanup. Take the tap out of the input path first.
    CGEvent::tap_enable(&tap, false);
    if let Some(token) = observer {
        unobserve_access_changes(token);
    }
    TAP_CTX.with(|c| c.set(std::ptr::null()));
    if let Some(timer) = &recheck_timer {
        timer.invalidate();
    }
    run_loop.remove_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CFMachPort::invalidate(&tap);
    unsafe {
        drop(Box::from_raw(ctx_ptr));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::macos::check_accessibility;
    use std::time::{Duration, Instant};

    use objc2_core_foundation::CFRunLoopTimer;

    unsafe extern "C-unwind" fn noop_timer(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {}

    /// The stop request must survive being issued before the run loop has
    /// been entered. A bare CFRunLoopStop is lost in that window (the stop
    /// flag lives in per-run data reset on entry), which would park the tap
    /// thread forever and hang KeyboardListener::drop. Needs no
    /// accessibility permission, so it runs everywhere including CI.
    #[test]
    fn stop_requested_before_run_entry_is_not_lost() {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let rl = CFRunLoop::current().expect("thread has a run loop");

            // A far-future timer keeps the mode non-empty so run_in_mode
            // actually blocks instead of returning Finished immediately.
            let timer = unsafe {
                CFRunLoopTimer::new(
                    None,
                    objc2_core_foundation::CFAbsoluteTimeGetCurrent() + 3600.0,
                    0.0,
                    0,
                    0,
                    Some(noop_timer),
                    std::ptr::null_mut(),
                )
            }
            .expect("create timer");
            rl.add_timer(Some(&timer), unsafe {
                objc2_core_foundation::kCFRunLoopDefaultMode
            });

            tx.send(TapRunLoop(rl.clone())).unwrap();

            // Guarantee the main thread's stop lands while this thread is
            // provably outside the run loop.
            thread::sleep(Duration::from_millis(200));

            let started = Instant::now();
            // Bounded run: if the stop request was lost this returns at the
            // 5s timeout and the elapsed assertion below fails, instead of
            // hanging the test.
            CFRunLoop::run_in_mode(
                unsafe { objc2_core_foundation::kCFRunLoopDefaultMode },
                5.0,
                false,
            );
            started.elapsed()
        });

        let run_loop = rx.recv().expect("run loop handle");
        run_loop.stop(); // lands before the thread enters the run loop

        let elapsed = handle.join().expect("tap thread panicked");
        assert!(
            elapsed < Duration::from_secs(2),
            "stop issued before run-loop entry was lost; loop ran for {elapsed:?}"
        );
    }

    unsafe extern "C-unwind" fn noop_callback(
        _proxy: CGEventTapProxy,
        _event_type: CGEventType,
        event: NonNull<CGEvent>,
        _user_info: *mut c_void,
    ) -> *mut CGEvent {
        event.as_ptr()
    }

    /// A real (listen-only) event tap, or None when the test runner has no
    /// accessibility permission (CI, unprivileged terminals) — tests skip.
    fn create_test_tap() -> Option<CFRetained<CFMachPort>> {
        if !check_accessibility() {
            eprintln!("SKIPPED: accessibility permission not granted to the test runner");
            return None;
        }
        unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                1 << CGEventType::KeyDown.0,
                Some(noop_callback),
                std::ptr::null_mut(),
            )
        }
    }

    fn make_ctx(tap: &CFRetained<CFMachPort>) -> CallbackCtx {
        make_ctx_trusting(tap, || true)
    }

    fn make_ctx_trusting(tap: &CFRetained<CFMachPort>, is_trusted: fn() -> bool) -> CallbackCtx {
        let (tx, _rx) = mpsc::channel();
        let ctx = CallbackCtx::new(
            Arc::new(Mutex::new(ListenerState::new(tx, None))),
            Arc::new(AtomicBool::new(false)),
            is_trusted,
        );
        ctx.tap.set(Some(NonNull::from(&**tap)));
        ctx
    }

    fn deliver_tap_disabled(ctx: &CallbackCtx, event_type: CGEventType) {
        let event = CGEvent::new(None).expect("failed to create CGEvent");
        let returned = unsafe {
            event_tap_callback(
                std::ptr::null_mut(),
                event_type,
                NonNull::from(&*event),
                ctx as *const CallbackCtx as *mut c_void,
            )
        };
        assert_eq!(
            returned,
            NonNull::from(&*event).as_ptr(),
            "pseudo-events must be passed through, not blocked"
        );
    }

    /// Recovery is event-driven now (no polling watchdog): receiving a
    /// TapDisabledBy* pseudo-event must re-enable the tap from the callback.
    #[test]
    fn tap_disabled_pseudo_event_reenables_tap() {
        let Some(tap) = create_test_tap() else { return };

        for event_type in [
            CGEventType::TapDisabledByTimeout,
            CGEventType::TapDisabledByUserInput,
        ] {
            let ctx = make_ctx(&tap);
            CGEvent::tap_enable(&tap, false);
            assert!(!CGEvent::tap_is_enabled(&tap));

            deliver_tap_disabled(&ctx, event_type);

            assert!(
                CGEvent::tap_is_enabled(&tap),
                "callback must re-enable the tap after {event_type:?}"
            );
        }
    }

    /// Re-enabling must not depend on the state mutex being healthy: a
    /// poisoned mutex (a panic on some other thread holding it) must not
    /// leave the tap permanently dead.
    #[test]
    fn tap_reenabled_even_with_poisoned_state_mutex() {
        let Some(tap) = create_test_tap() else { return };

        let ctx = make_ctx(&tap);
        let state = Arc::clone(&ctx.state);
        let _ = thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("poison the listener state mutex");
        })
        .join();
        assert!(ctx.state.lock().is_err(), "mutex should be poisoned");

        CGEvent::tap_enable(&tap, false);
        assert!(!CGEvent::tap_is_enabled(&tap));

        deliver_tap_disabled(&ctx, CGEventType::TapDisabledByTimeout);

        assert!(
            CGEvent::tap_is_enabled(&tap),
            "re-enable must happen even when the state lock is unusable"
        );
    }

    /// The pseudo-event handler must reconcile modifier state (the original
    /// purpose of PR #4): stale tracked modifiers with no physically-held
    /// keys must be cleared.
    #[test]
    fn tap_disabled_pseudo_event_reconciles_modifiers() {
        let Some(tap) = create_test_tap() else { return };

        let ctx = make_ctx(&tap);
        ctx.state.lock().unwrap().current_modifiers = Modifiers::CTRL_LEFT | Modifiers::SHIFT_RIGHT;

        deliver_tap_disabled(&ctx, CGEventType::TapDisabledByTimeout);

        // No modifier keys are physically held while tests run, so
        // reconciliation against OS flags must clear the stale state.
        assert_eq!(
            ctx.state.lock().unwrap().current_modifiers,
            Modifiers::empty(),
            "stale modifiers must be reconciled away on tap-disable"
        );
    }

    // ---- Accessibility permission revoked while running ----

    /// The probe agrees with AXIsProcessTrusted while that one is fresh (at
    /// startup), with or without the permission.
    #[test]
    fn active_tap_probe_matches_the_permission() {
        assert_eq!(may_run_active_tap(), check_accessibility());
    }

    #[test]
    fn access_change_subscription_registers_and_cancels() {
        let run_loop = CFRunLoop::current().expect("run loop");
        let token = observe_access_changes(&run_loop).expect("notify registration");
        unobserve_access_changes(token);
    }

    #[test]
    fn access_watch_decision_table() {
        use AccessAction::*;
        let mut watch = AccessWatch::default();
        assert_eq!(watch.observe(true), Enable);
        // One "not trusted" may be a stale answer: pause, don't stop.
        assert_eq!(watch.observe(false), PauseAndRecheck);
        // Trusted again: the strike is forgotten.
        assert_eq!(watch.observe(true), Enable);
        assert_eq!(watch.observe(false), PauseAndRecheck);
        // Two in a row: revoked.
        assert_eq!(watch.observe(false), Stop);
        assert_eq!(watch.observe(false), Stop);
    }

    /// While the permission is gone, a TapDisabledBy* pseudo-event must not
    /// put the tap back in front of all input (that froze the Mac); the
    /// second "not trusted" answer stops the listener.
    #[test]
    fn tap_stays_disabled_when_permission_revoked() {
        let Some(tap) = create_test_tap() else { return };

        for event_type in [
            CGEventType::TapDisabledByTimeout,
            CGEventType::TapDisabledByUserInput,
        ] {
            let ctx = make_ctx_trusting(&tap, || false);
            CGEvent::tap_enable(&tap, false);

            deliver_tap_disabled(&ctx, event_type);
            assert!(
                !CGEvent::tap_is_enabled(&tap),
                "untrusted: the tap must stay disabled after {event_type:?}"
            );
            assert!(
                !ctx.accessibility_revoked.load(Ordering::SeqCst),
                "a single untrusted answer must not stop the listener"
            );

            // The delayed re-check (normally the timer) confirms it.
            unsafe { access_recheck_fired(std::ptr::null_mut(), &ctx as *const _ as *mut c_void) };
            assert!(!CGEvent::tap_is_enabled(&tap));
            assert!(ctx.accessibility_revoked.load(Ordering::SeqCst));
        }
    }

    thread_local! {
        static FAKE_TRUSTED: Cell<bool> = const { Cell::new(true) };
    }
    fn fake_trusted() -> bool {
        FAKE_TRUSTED.with(Cell::get)
    }

    /// A brief false "not trusted" (around a settings
    /// change) pauses the tap, and the re-check puts it back.
    #[test]
    fn stale_untrusted_answer_only_pauses_the_tap() {
        let Some(tap) = create_test_tap() else { return };
        let ctx = make_ctx_trusting(&tap, fake_trusted);
        CGEvent::tap_enable(&tap, true);

        FAKE_TRUSTED.with(|t| t.set(false));
        ctx.on_access_changed();
        assert!(!CGEvent::tap_is_enabled(&tap), "paused while unsure");

        FAKE_TRUSTED.with(|t| t.set(true));
        unsafe { access_recheck_fired(std::ptr::null_mut(), &ctx as *const _ as *mut c_void) };
        assert!(CGEvent::tap_is_enabled(&tap), "re-enabled once trusted");
        assert!(!ctx.accessibility_revoked.load(Ordering::SeqCst));
    }

    /// The same on the permission-free path (no tap): what decides stopping
    /// is the revoked flag.
    #[test]
    fn untrusted_twice_sets_revoked_without_tap() {
        let (tx, _rx) = mpsc::channel();
        let ctx = CallbackCtx::new(
            Arc::new(Mutex::new(ListenerState::new(tx, None))),
            Arc::new(AtomicBool::new(false)),
            || false,
        );
        ctx.on_access_changed();
        assert!(!ctx.accessibility_revoked.load(Ordering::SeqCst));
        unsafe { access_recheck_fired(std::ptr::null_mut(), &ctx as *const _ as *mut c_void) };
        assert!(ctx.accessibility_revoked.load(Ordering::SeqCst));
    }

    static E2E_TRUSTED: AtomicBool = AtomicBool::new(true);
    fn e2e_trusted() -> bool {
        E2E_TRUSTED.load(Ordering::SeqCst)
    }

    /// End to end on the real tap thread: an accessibility change that
    /// revoked the permission stops the run loop through the re-check timer,
    /// tears the tap down and disconnects the event channel.
    #[test]
    fn revocation_stops_the_tap_thread() {
        if !check_accessibility() {
            eprintln!("SKIPPED: accessibility permission not granted to the test runner");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(ListenerState::new(tx, None)));
        let running = Arc::new(AtomicBool::new(true));
        let revoked = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = mpsc::channel();
        let (r, v) = (Arc::clone(&running), Arc::clone(&revoked));
        let handle = thread::spawn(move || run_event_tap(state, r, v, e2e_trusted, init_tx));
        let run_loop = init_rx.recv().unwrap().expect("tap created");

        // Trusted: a notification changes nothing.
        run_loop.notify_access_changed();
        thread::sleep(Duration::from_millis(800));
        assert!(!handle.is_finished());

        E2E_TRUSTED.store(false, Ordering::SeqCst);
        let started = Instant::now();
        run_loop.notify_access_changed();
        while !handle.is_finished() && started.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            handle.is_finished(),
            "tap thread must stop after revocation"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "must wait for the confirming re-check, stopped after {elapsed:?}"
        );
        handle.join().unwrap();
        assert!(revoked.load(Ordering::SeqCst));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    // ---- Per-press blocking (cjpais/Handy#2228) ----
    //
    // These drive the real tap callback with synthetic CGEvents, so they need
    // no accessibility permission and run everywhere.

    const VK_SPACE: u16 = 0x31;
    const VK_A: u16 = 0x00;

    /// Super-key style modifiers: added to key events' flags, with no
    /// FlagsChanged events (what Vorssaint and similar remappers do).
    fn hyper() -> CGEventFlags {
        CGEventFlags::MaskControl
            | CGEventFlags::MaskAlternate
            | CGEventFlags::MaskShift
            | CGEventFlags::MaskCommand
    }

    fn blocking_ctx(hotkeys: &[&str]) -> (CallbackCtx, Receiver<KeyEvent>, BlockingHotkeys) {
        let (tx, rx) = mpsc::channel();
        let set: BlockingHotkeys = Arc::new(Mutex::new(
            hotkeys
                .iter()
                .map(|h| h.parse().expect("hotkey parses"))
                .collect(),
        ));
        let ctx = CallbackCtx::new(
            Arc::new(Mutex::new(ListenerState::new(tx, Some(Arc::clone(&set))))),
            Arc::new(AtomicBool::new(false)),
            check_accessibility,
        );
        (ctx, rx, set)
    }

    /// Deliver one keyboard event to the callback; returns whether it was blocked.
    fn send_key(ctx: &CallbackCtx, keycode: u16, phase: PressPhase, flags: CGEventFlags) -> bool {
        let event = CGEvent::new_keyboard_event(None, keycode, phase != PressPhase::Up)
            .expect("failed to create keyboard event");
        CGEvent::set_flags(Some(&event), flags);
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::KeyboardEventAutorepeat,
            i64::from(phase == PressPhase::Repeat),
        );
        let event_type = if phase == PressPhase::Up {
            CGEventType::KeyUp
        } else {
            CGEventType::KeyDown
        };
        let returned = unsafe {
            event_tap_callback(
                std::ptr::null_mut(),
                event_type,
                NonNull::from(&*event),
                ctx as *const CallbackCtx as *mut c_void,
            )
        };
        returned.is_null()
    }

    use PressPhase::{Down, Repeat, Up};

    #[test]
    fn press_blocking_decision_table() {
        let mut p = PressBlocking::default();
        // Blocked press: repeats and key-up follow it.
        assert!(p.decide(1, Down, true));
        assert!(p.decide(1, Repeat, false));
        assert!(p.decide(1, Up, false));
        // Passed press: repeats are judged on their own; key-up follows it.
        assert!(!p.decide(1, Down, false));
        assert!(!p.decide(1, Repeat, false));
        assert!(p.decide(1, Repeat, true));
        assert!(!p.decide(1, Up, true));
        // No recorded press: judged on its own, as before.
        assert!(p.decide(2, Repeat, true));
        assert!(!p.decide(2, Repeat, false));
        assert!(p.decide(2, Up, true));
        assert!(!p.decide(2, Up, false));
        // Key-up consumes the decision.
        assert!(p.decide(3, Down, true));
        assert!(p.decide(3, Up, false));
        assert!(!p.decide(3, Up, false));
        // Keys are independent.
        assert!(p.decide(4, Down, true));
        assert!(!p.decide(5, Down, false));
        assert!(p.decide(4, Repeat, false));
        assert!(!p.decide(5, Repeat, false));
        // forget_all drops everything.
        p.forget_all();
        assert!(p.decided.is_empty());
    }

    /// cjpais/Handy#2228: the modifiers vanish mid-hold (Vorssaint's watchdog
    /// stops adding them). The hotkey is still pressed, so its key's repeats
    /// and key-up must stay blocked instead of typing into the focused app.
    #[test]
    fn modifiers_dropping_mid_hold_keep_the_press_blocked() {
        let (ctx, _rx, _set) = blocking_ctx(&["Ctrl+Opt+Shift+Cmd+Space"]);
        assert!(send_key(&ctx, VK_SPACE, Down, hyper()));
        assert!(send_key(&ctx, VK_SPACE, Repeat, hyper()));
        assert!(
            send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::empty()),
            "repeat leaked"
        );
        assert!(
            send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::empty()),
            "repeat leaked"
        );
        assert!(
            send_key(&ctx, VK_SPACE, Up, CGEventFlags::empty()),
            "key-up leaked"
        );
    }

    /// Releasing the modifier before the key: the key-up belongs to a blocked
    /// press and must not reach the app as an orphan.
    #[test]
    fn modifier_released_first_does_not_leak_key_up() {
        let (ctx, _rx, _set) = blocking_ctx(&["Cmd+Space"]);
        assert!(send_key(&ctx, VK_SPACE, Down, CGEventFlags::MaskCommand));
        assert!(send_key(&ctx, VK_SPACE, Up, CGEventFlags::empty()));
    }

    /// Space pressed just before Opt, then held: the repeats match the hotkey
    /// (the manager fires on them) and are blocked, but the key-up passes,
    /// since the app saw the key-down and would otherwise keep the key down.
    #[test]
    fn key_before_modifier_blocks_matching_repeats_but_not_key_up() {
        let (ctx, _rx, _set) = blocking_ctx(&["Opt+Space"]);
        assert!(!send_key(&ctx, VK_SPACE, Down, CGEventFlags::empty()));
        assert!(!send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::empty()));
        assert!(send_key(
            &ctx,
            VK_SPACE,
            Repeat,
            CGEventFlags::MaskAlternate
        ));
        assert!(send_key(
            &ctx,
            VK_SPACE,
            Repeat,
            CGEventFlags::MaskAlternate
        ));
        assert!(!send_key(&ctx, VK_SPACE, Up, CGEventFlags::MaskAlternate));
    }

    /// The ordinary case is unchanged.
    #[test]
    fn plain_hotkey_press_is_blocked_and_other_keys_pass() {
        let (ctx, _rx, _set) = blocking_ctx(&["Cmd+Space"]);
        assert!(send_key(&ctx, VK_SPACE, Down, CGEventFlags::MaskCommand));
        assert!(send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::MaskCommand));
        assert!(send_key(&ctx, VK_SPACE, Up, CGEventFlags::MaskCommand));

        assert!(!send_key(&ctx, VK_A, Down, CGEventFlags::MaskCommand));
        assert!(!send_key(&ctx, VK_A, Up, CGEventFlags::MaskCommand));
        assert!(!send_key(&ctx, VK_SPACE, Down, CGEventFlags::empty()));
        assert!(!send_key(&ctx, VK_SPACE, Up, CGEventFlags::empty()));
    }

    /// A press whose key-down we never saw (listener started mid-hold) has no
    /// decision; its events are judged on their own modifiers, as before.
    #[test]
    fn unseen_press_falls_back_to_modifier_matching() {
        let (ctx, _rx, _set) = blocking_ctx(&["Cmd+Space"]);
        assert!(send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::MaskCommand));
        assert!(!send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::empty()));
        assert!(!send_key(&ctx, VK_SPACE, Up, CGEventFlags::empty()));
    }

    /// A missed key-up must not decide a later press: every fresh key-down
    /// decides anew.
    #[test]
    fn fresh_press_decides_anew_after_a_missed_key_up() {
        let (ctx, _rx, _set) = blocking_ctx(&["Cmd+Space"]);
        assert!(send_key(&ctx, VK_SPACE, Down, CGEventFlags::MaskCommand));
        // (key-up lost)
        assert!(!send_key(&ctx, VK_SPACE, Down, CGEventFlags::empty()));
        assert!(!send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::empty()));
        assert!(!send_key(&ctx, VK_SPACE, Up, CGEventFlags::empty()));
    }

    /// While the tap was disabled events may have been missed, so decisions
    /// are forgotten and events fall back to modifier matching.
    #[test]
    fn tap_disable_forgets_press_decisions() {
        let (ctx, _rx, _set) = blocking_ctx(&["Cmd+Space"]);
        assert!(send_key(&ctx, VK_SPACE, Down, CGEventFlags::MaskCommand));
        deliver_tap_disabled(&ctx, CGEventType::TapDisabledByTimeout);
        assert!(ctx.presses.borrow().decided.is_empty());
        assert!(!send_key(&ctx, VK_SPACE, Up, CGEventFlags::empty()));
    }

    /// Unregistering a hotkey mid-press still blocks that press's key-up, so
    /// the app never sees a key-up without its key-down.
    #[test]
    fn hotkey_unregistered_mid_press_still_blocks_its_key_up() {
        let (ctx, _rx, set) = blocking_ctx(&["Cmd+Space"]);
        assert!(send_key(&ctx, VK_SPACE, Down, CGEventFlags::MaskCommand));
        set.lock().unwrap().clear();
        assert!(send_key(&ctx, VK_SPACE, Up, CGEventFlags::MaskCommand));
        // The next press sees the new hotkey set.
        assert!(!send_key(&ctx, VK_SPACE, Down, CGEventFlags::MaskCommand));
    }

    /// Only blocking changed: the events reported to the manager are the same
    /// as before, modifiers included.
    #[test]
    fn reported_events_are_unchanged() {
        let (ctx, rx, _set) = blocking_ctx(&["Cmd+Space"]);
        send_key(&ctx, VK_SPACE, Down, CGEventFlags::MaskCommand);
        send_key(&ctx, VK_SPACE, Repeat, CGEventFlags::empty());
        send_key(&ctx, VK_SPACE, Up, CGEventFlags::empty());
        let events: Vec<_> = rx
            .try_iter()
            .map(|e| (e.key, e.is_key_down, e.modifiers))
            .collect();
        assert_eq!(
            events,
            vec![
                (Some(Key::Space), true, Modifiers::CMD_LEFT),
                (Some(Key::Space), true, Modifiers::empty()),
                (Some(Key::Space), false, Modifiers::empty()),
            ]
        );
    }

    const KC_A: u16 = 0x00;
    const KC_RIGHT_CTRL: u16 = 0x3E;
    const FLAG_CONTROL: u64 = CGEventFlags::MaskControl.0;
    const FLAG_COMMAND: u64 = CGEventFlags::MaskCommand.0;
    const HARDWARE: i64 = 0;

    fn own() -> i64 {
        i64::from(std::process::id())
    }

    fn make_ctx_with_events() -> (CallbackCtx, Receiver<KeyEvent>) {
        let (tx, rx) = mpsc::channel();
        let ctx = CallbackCtx::new(
            Arc::new(Mutex::new(ListenerState::new(tx, None))),
            Arc::new(AtomicBool::new(false)),
            check_accessibility,
        );
        (ctx, rx)
    }

    /// Run one keyboard event through the tap callback and return what it
    /// emitted. The event is never posted, so no accessibility permission is
    /// needed.
    fn deliver(
        ctx: &CallbackCtx,
        rx: &Receiver<KeyEvent>,
        event_type: CGEventType,
        keycode: u16,
        flags: u64,
        source_pid: i64,
    ) -> Vec<KeyEvent> {
        let event = CGEvent::new_keyboard_event(None, keycode, true).expect("create CGEvent");
        CGEvent::set_type(Some(&event), event_type);
        CGEvent::set_flags(Some(&event), CGEventFlags(flags));
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUnixProcessID,
            source_pid,
        );
        unsafe {
            event_tap_callback(
                std::ptr::null_mut(),
                event_type,
                NonNull::from(&*event),
                ctx as *const CallbackCtx as *mut c_void,
            );
        }
        rx.try_iter().collect()
    }

    /// Text we type with enigo (flags forced to zero) while the user holds a
    /// modifier-only hotkey must not clear that modifier, or the release is
    /// reported as a press (cjpais/Handy#2501).
    #[test]
    fn own_posted_key_event_does_not_clear_a_held_modifier() {
        let (ctx, rx) = make_ctx_with_events();
        let flags_changed = CGEventType::FlagsChanged;

        let down = deliver(
            &ctx,
            &rx,
            flags_changed,
            KC_RIGHT_CTRL,
            FLAG_CONTROL,
            HARDWARE,
        );
        assert!(down[0].is_key_down);

        deliver(&ctx, &rx, CGEventType::KeyDown, KC_A, 0, own());
        assert_eq!(
            ctx.state.lock().unwrap().current_modifiers,
            Modifiers::CTRL_RIGHT
        );

        let up = deliver(&ctx, &rx, flags_changed, KC_RIGHT_CTRL, 0, HARDWARE);
        assert_eq!(up.len(), 1);
        assert!(!up[0].is_key_down);
        assert_eq!(up[0].modifiers, Modifiers::empty());
    }

    /// Our own posted events are still reported with their own flags.
    #[test]
    fn own_posted_key_event_reports_modifiers_from_its_own_flags() {
        let (ctx, rx) = make_ctx_with_events();
        deliver(
            &ctx,
            &rx,
            CGEventType::FlagsChanged,
            KC_RIGHT_CTRL,
            FLAG_CONTROL,
            HARDWARE,
        );

        let plain = deliver(&ctx, &rx, CGEventType::KeyDown, KC_A, 0, own());
        assert_eq!(plain[0].modifiers, Modifiers::empty());

        let with_cmd = deliver(&ctx, &rx, CGEventType::KeyDown, KC_A, FLAG_COMMAND, own());
        assert_eq!(with_cmd[0].modifiers, Modifiers::CMD_LEFT);
    }

    /// Events from any other source still reconcile tracked state: a stale
    /// modifier is cleared and a missed press is filled in.
    #[test]
    fn other_key_events_still_reconcile_modifiers() {
        let (ctx, rx) = make_ctx_with_events();
        let other = own() + 1;
        ctx.state.lock().unwrap().current_modifiers = Modifiers::SHIFT_LEFT;

        deliver(&ctx, &rx, CGEventType::KeyDown, KC_A, FLAG_CONTROL, other);
        assert_eq!(
            ctx.state.lock().unwrap().current_modifiers,
            Modifiers::CTRL_LEFT
        );
    }
}
