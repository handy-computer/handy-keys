//! Guards against recurring WindowServer IPC from the macOS listener.
//!
//! handy-keys ≤ 0.3.2 polled CGEventTapIsEnabled every 100ms from the event
//! tap run loop. Each call is a synchronous mach RPC to WindowServer; on
//! macOS 26 those RPCs leaked kernel IPC vouchers until the machine kernel-
//! panicked with "Cannot grow ipc space beyond IVAC_ENTRIES_MAX"
//! (cjpais/Handy#1827). The fix made tap recovery event-driven, so an idle
//! listener must send (nearly) zero mach messages.
//!
//! This test measures the task-wide `messages_sent` counter around a 5-second
//! idle window. The old watchdog alone produced ≥50 sends in that window;
//! the threshold below fails loudly if recurring IPC ever creeps back in.
//!
//! Requires accessibility permission (skips itself otherwise). Avoid typing
//! or clicking while it runs: every tapped input event costs a couple of
//! mach messages and inflates the measurement.

#![cfg(target_os = "macos")]

use std::sync::Mutex;
use std::time::Duration;

use handy_keys::KeyboardListener;

/// Tests in this file measure task-wide IPC counters, so they must not run
/// concurrently with each other (cargo runs same-binary tests in parallel by
/// default and the churn in `shutdown_is_prompt` would pollute the idle
/// measurement).
static SERIAL: Mutex<()> = Mutex::new(());

/// `struct task_events_info` from <mach/task_info.h>.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct TaskEventsInfo {
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    csw: i32,
}

const TASK_EVENTS_INFO: u32 = 2;

extern "C" {
    static mach_task_self_: u32;
    fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
}

fn messages_sent() -> i64 {
    let mut info = TaskEventsInfo::default();
    let mut count = (std::mem::size_of::<TaskEventsInfo>() / 4) as u32;
    let kr = unsafe {
        task_info(
            mach_task_self_,
            TASK_EVENTS_INFO,
            &mut info as *mut TaskEventsInfo as *mut i32,
            &mut count,
        )
    };
    assert_eq!(kr, 0, "task_info(TASK_EVENTS_INFO) failed: {kr}");
    info.messages_sent as i64
}

#[test]
fn idle_listener_sends_no_recurring_ipc() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let listener = match KeyboardListener::new() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("SKIPPED: cannot create listener ({e})");
            return;
        }
    };

    // Let startup IPC (tap creation, first enable) settle.
    std::thread::sleep(Duration::from_millis(500));

    // Real keyboard/mouse activity during a window inflates the count (each
    // tapped event costs mach messages), but nothing can deflate one — so
    // the *minimum* over several windows filters desktop-activity noise
    // without ever masking real recurring IPC: the old 10Hz watchdog put a
    // ≥100-message floor under every window, far above the threshold.
    let mut best = i64::MAX;
    for window in 1..=3 {
        let before = messages_sent();
        std::thread::sleep(Duration::from_secs(5));
        let delta = messages_sent() - before;
        best = best.min(delta);
        println!("window {window}: {delta} mach messages sent during 5s idle");
        if best < 25 {
            break;
        }
    }

    assert!(
        best < 25,
        "idle listener sent ≥{best} mach messages in every 5s window — \
         recurring IPC (e.g. a CGEventTapIsEnabled watchdog) has crept back \
         in; that pattern kernel-panics macOS (cjpais/Handy#1827)"
    );

    drop(listener);
}

/// Shutdown must not hang: the listener thread parks in CFRunLoopRun with no
/// periodic wakeups, so Drop's CFRunLoopStop is the only thing that ends it.
/// Guards the stop-flag handoff (including the stop-before-run-entry race).
#[test]
fn shutdown_is_prompt() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    for _ in 0..5 {
        let listener = match KeyboardListener::new() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("SKIPPED: cannot create listener ({e})");
                return;
            }
        };
        let start = std::time::Instant::now();
        drop(listener);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "listener drop took {elapsed:?} — tap thread failed to stop promptly"
        );
    }
}
