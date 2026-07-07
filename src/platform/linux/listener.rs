//! Linux keyboard listener reading evdev devices directly
//!
//! A single thread polls every keyboard-capable `/dev/input/event*` device
//! plus an inotify watch on `/dev/input` for hotplug. Reading evdev
//! directly needs no X11 or Wayland connection, so the listener works
//! identically on both and on a bare console; it requires read access to
//! the device nodes (typically membership in the `input` group).
//!
//! # Blocking
//!
//! Without blocking, devices are opened non-exclusively — nothing is
//! grabbed, nothing is injected. With blocking enabled, every *keyboard*
//! is grabbed exclusively (EVIOCGRAB) and its events re-injected through a
//! per-device uinput clone, minus the key events (and their MSC_SCAN
//! companions) that match a blocking hotkey. Pointer devices are never
//! grabbed (their buttons are observed read-only, so mouse-button hotkeys
//! are detected but not blocked — matching the Windows backend). Blocking
//! therefore additionally needs write access to `/dev/uinput`, verified at
//! spawn.
//!
//! A keyboard is only grabbed while quiet: grabbing mid-keypress would
//! split the press/release pair across two devices and wedge the key at
//! the compositor (libinput discards unbalanced key-ups). Devices with
//! keys held stay read-only (`pending_grab`) until EVIOCGKEY reports them
//! quiet, and a grab that races a fresh keypress is backed out and
//! retried. Events observed before a grab lands are processed for hotkey
//! detection but not re-injected — the compositor received them natively.
//!
//! Each listener tracks the device nodes of its own uinput clones and
//! filters exactly those from its enumeration (reading one's own output
//! would loop). Other listeners — including in other processes — read the
//! clones like ordinary keyboards, so a listener stacked behind a blocking
//! one still sees every non-blocked event. Blocking listeners, however,
//! never *grab* each other's clones (recognized by name prefix): two
//! blockers cloning each other's clones would mint new uinput devices
//! without bound. First blocker wins; later ones detect but cannot block
//! what it already grabbed.
//!
//! # Modifier- and key-state recovery
//!
//! Tracked modifiers are seeded from the kernel's key state (EVIOCGKEY) at
//! startup and re-reconciled against it whenever the kernel reports a
//! dropped-buffer overflow (`SYN_DROPPED`) or a device disappears while
//! keys are held. Stale modifiers are cleared with synthetic release
//! events, sharing `release_events` with the Windows listener. Held
//! non-modifier keys get synthetic channel releases too, and after an
//! overflow the uinput clone's kernel state is repaired by re-injecting
//! releases for everything no longer physically held (the input core
//! silently drops the no-ops).
//!
//! # Shutdown
//!
//! Dropping the listener flips the shared `running` flag; the poll loop
//! wakes at least every `POLL_TIMEOUT_MS`, observes it, and exits. Closing
//! the device fds releases every grab, and dropping the uinput clones
//! destroys them (the kernel releases any keys held inside a destroyed
//! device).

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use evdev::raw_stream::RawDevice;
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsoluteAxisCode, EventSummary, EventType, InputEvent, KeyCode, RelativeAxisCode,
    SynchronizationCode,
};

use crate::error::{Error, Result};
use crate::platform::state::{release_events, stale_modifiers, RECONCILABLE};
use crate::platform::state::{BlockingHotkeys, ListenerState};
use crate::types::{Key, KeyEvent, Modifiers};

use super::keycode::{key_code_to_key, key_code_to_modifier};

const DEV_INPUT: &str = "/dev/input";

/// Upper bound on how long shutdown, hotplug pickup, and deferred-grab
/// completion can lag: the poll loop re-checks these at least this often.
const POLL_TIMEOUT_MS: i32 = 100;

/// How long to keep retrying a hotplugged node that appeared but could not
/// be opened yet (udev applies permissions asynchronously, and not every
/// grant produces another inotify event).
const HOTPLUG_RETRY_WINDOW: Duration = Duration::from_secs(2);

/// Name prefix of the uinput re-injection clones. Doubles as the marker by
/// which blocking listeners recognize each other's outputs and refuse to
/// grab them (see `is_passthrough_clone`).
const CLONE_NAME_PREFIX: &str = "handy-keys passthrough: ";

/// The side-specific modifier keys we track, paired with their evdev
/// codes. Ordered to match `state::MODIFIER_RELEASE_ORDER`.
const MODIFIER_KEYS: [(KeyCode, Modifiers); 8] = [
    (KeyCode::KEY_LEFTMETA, Modifiers::CMD_LEFT),
    (KeyCode::KEY_RIGHTMETA, Modifiers::CMD_RIGHT),
    (KeyCode::KEY_LEFTSHIFT, Modifiers::SHIFT_LEFT),
    (KeyCode::KEY_RIGHTSHIFT, Modifiers::SHIFT_RIGHT),
    (KeyCode::KEY_LEFTCTRL, Modifiers::CTRL_LEFT),
    (KeyCode::KEY_RIGHTCTRL, Modifiers::CTRL_RIGHT),
    (KeyCode::KEY_LEFTALT, Modifiers::OPT_LEFT),
    (KeyCode::KEY_RIGHTALT, Modifiers::OPT_RIGHT),
];

// Key event values from the kernel input protocol.
const KEY_RELEASED: i32 = 0;
const KEY_PRESSED: i32 = 1;
const KEY_AUTOREPEAT: i32 = 2;

/// Blocking state for one listener instance.
struct GrabConfig {
    /// Device nodes of this listener's own uinput clones, excluded from
    /// its enumeration: reading one's own output would loop. Entries are
    /// removed when the backing device (and thus its clone) goes away, so
    /// a later reuse of the node path by real hardware is not filtered.
    own_outputs: HashSet<PathBuf>,
}

/// Listener-wide event-processing state.
struct LinuxState {
    listener: ListenerState,
    /// Modifiers whose press was blocked; the matching release (and any
    /// duplicate press/release from a second keyboard) follows the same
    /// decision, so the re-injected stream never carries half of a
    /// press/release pair. The reverse also holds: a modifier whose press
    /// was forwarded is never blocked on release, even if a matching
    /// hotkey was registered mid-hold — otherwise the compositor would see
    /// a press with no release and stick the key.
    blocked_modifiers: Modifiers,
}

/// An input device the listener is reading.
struct OpenDevice {
    path: PathBuf,
    dev: RawDevice,
    /// The kernel reported SYN_DROPPED: events were lost. Per the evdev
    /// protocol, everything up to and including the next SYN_REPORT is
    /// unreliable for state tracking; key and modifier state are then
    /// repaired against EVIOCGKEY. (The unreliable events are still
    /// forwarded to a grabbed device's clone — the user really typed
    /// them.)
    resyncing: bool,
    /// Present when this device is grabbed for blocking: the uinput clone
    /// through which non-blocked events are re-injected.
    output: Option<VirtualDevice>,
    /// The clone's /dev/input nodes, held to unregister them from
    /// `GrabConfig::own_outputs` when this device is removed.
    output_nodes: Vec<PathBuf>,
    /// This keyboard should be grabbed but hasn't been yet — either it was
    /// just opened, or keys were physically held (see module doc). The
    /// poll loop completes the grab at the first quiet moment.
    pending_grab: bool,
    /// Events of the in-flight batch, forwarded to `output` as one unit on
    /// SYN_REPORT so batching is preserved. Unused when not grabbed.
    pending: Vec<InputEvent>,
    /// Non-modifier keys whose press was blocked on THIS device. Their
    /// autorepeats and release are blocked too, so the re-injected stream
    /// never carries an orphan repeat or key-up. Per-device: the same key
    /// code held on two keyboards must pair press/release independently.
    blocked_keys: HashSet<KeyCode>,
    /// Mappable non-modifier keys currently physically held on this
    /// device, per the events we processed. Used to synthesize channel
    /// releases when the device vanishes mid-press or an overflow eats a
    /// key-up, so consumers' press/release state machines recover.
    keys_down: HashSet<KeyCode>,
}

/// Internal listener state returned to KeyboardListener
pub(crate) struct LinuxListenerState {
    pub event_receiver: Receiver<KeyEvent>,
    pub thread_handle: Option<JoinHandle<()>>,
    pub running: Arc<AtomicBool>,
    pub blocking_hotkeys: Option<BlockingHotkeys>,
}

/// Spawn an evdev keyboard listener for Linux
///
/// Fails if no input device can be opened, or if blocking is requested but
/// `/dev/uinput` is not writable — reported with the fix in the error
/// message — so a caller can tell hotkeys will actually work before this
/// returns Ok.
pub(crate) fn spawn(blocking_hotkeys: Option<BlockingHotkeys>) -> Result<LinuxListenerState> {
    let mut grab = blocking_hotkeys.as_ref().map(|_| GrabConfig {
        own_outputs: HashSet::new(),
    });
    if grab.is_some() {
        check_uinput_access()?;
    }

    // Watch for hotplug *before* scanning: a node that appears (or becomes
    // readable via udev's chmod) during the scan then shows up as a pending
    // inotify event for the first poll instead of falling into the gap.
    let inotify = Inotify::watch(DEV_INPUT).map_err(|e| {
        Error::Platform(format!(
            "failed to watch {DEV_INPUT} for device hotplug: {e}"
        ))
    })?;
    let devices = open_capturable_devices(&mut grab)?;

    let (tx, rx) = mpsc::channel();
    let mut state = LinuxState {
        listener: ListenerState::new(tx, blocking_hotkeys.clone()),
        blocked_modifiers: Modifiers::empty(),
    };
    // Adopt modifiers physically held right now (e.g. the listener was
    // started from a hotkey press) so they ride along on the next event.
    state.listener.current_modifiers = physical_modifiers(&devices);

    let running = Arc::new(AtomicBool::new(true));
    let thread_running = Arc::clone(&running);

    let handle = thread::spawn(move || {
        run_loop(devices, inotify, grab, state, thread_running);
    });

    Ok(LinuxListenerState {
        event_receiver: rx,
        thread_handle: Some(handle),
        running,
        blocking_hotkeys,
    })
}

/// Verify /dev/uinput is usable before promising blocking.
fn check_uinput_access() -> Result<()> {
    // VirtualDevice::builder() opens /dev/uinput; nothing is created yet.
    VirtualDevice::builder().map(|_| ()).map_err(|e| {
        Error::Platform(format!(
            "hotkey blocking requires write access to /dev/uinput (non-blocked keystrokes are \
             re-injected through it): {e}. Grant it with a udev rule (e.g. KERNEL==\"uinput\", \
             GROUP=\"input\", MODE=\"0660\" plus input-group membership), or use the non-blocking \
             listener"
        ))
    })
}

/// Open every capturable device under /dev/input, or explain why none could
/// be opened.
fn open_capturable_devices(grab: &mut Option<GrabConfig>) -> Result<Vec<OpenDevice>> {
    let entries = fs::read_dir(DEV_INPUT)
        .map_err(|e| Error::Platform(format!("cannot read {DEV_INPUT}: {e}")))?;

    let mut devices = Vec::new();
    let mut permission_denied = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_event_node(&path) {
            continue;
        }
        match open_device(&path, grab) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) => {} // no keys or buttons we can map (e.g. accelerometers)
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => permission_denied += 1,
            Err(_) => {} // transient (device unplugged mid-scan, etc.)
        }
    }

    if devices.is_empty() {
        return Err(Error::Platform(if permission_denied > 0 {
            format!(
                "permission denied opening {permission_denied} device node(s) under {DEV_INPUT}. \
                 Reading keyboard events requires read access to these nodes: add your user to \
                 the 'input' group (sudo usermod -aG input $USER) and log out and back in"
            )
        } else {
            format!("no keyboard-capable input devices found under {DEV_INPUT}")
        }));
    }

    Ok(devices)
}

fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("event"))
}

/// Open one device node, returning Ok(None) if it can't produce any event
/// we can map, or if it is one of this listener's own uinput clones.
///
/// Never grabs: in blocking mode, grabbable keyboards are only *marked*
/// (`pending_grab`) and the poll loop completes the grab at the first
/// quiet moment (see `complete_grab`).
fn open_device(path: &Path, grab: &mut Option<GrabConfig>) -> io::Result<Option<OpenDevice>> {
    if let Some(cfg) = grab {
        if cfg.own_outputs.contains(path) {
            return Ok(None); // our own clone: reading it back would loop
        }
    }
    let dev = RawDevice::open(path)?;
    if !is_capturable(&dev) {
        return Ok(None);
    }
    // Non-blocking so the poll loop can drain each readable device fully
    // without ever sleeping inside a read.
    set_nonblocking(dev.as_raw_fd())?;

    let mut pending_grab = false;
    if grab.is_some() {
        if is_passthrough_clone(&dev) {
            // Another blocking listener's re-injection output. Reading it
            // gives us its keyboard's (non-blocked) events; grabbing it
            // would make that listener see OUR clone as a new keyboard and
            // clone it back — an unbounded device storm. First blocker
            // wins; we detect but cannot block behind it.
            eprintln!(
                "handy-keys: {} is another handy-keys blocking listener's output; its hotkeys \
                 will be detected but not blocked by this listener",
                path.display()
            );
        } else if is_keyboard(&dev) {
            pending_grab = true;
        } else if has_keyboard_keys(&dev) {
            // Combo device exposing keyboard keys on a pointer node: the
            // no-grabbing-pointers rule wins, but say so — "why isn't my
            // hotkey blocked" is undebuggable otherwise.
            eprintln!(
                "handy-keys: {} exposes keyboard keys on a pointer device; its hotkeys will be \
                 detected but not blocked (pointer devices are never grabbed)",
                path.display()
            );
        }
    }

    Ok(Some(OpenDevice {
        path: path.to_path_buf(),
        dev,
        resyncing: false,
        output: None,
        output_nodes: Vec::new(),
        pending_grab,
        pending: Vec::new(),
        blocked_keys: HashSet::new(),
        keys_down: HashSet::new(),
    }))
}

/// A device is worth reading iff it can emit at least one key or button we
/// can translate: keyboards and keypads via `key_code_to_key`/modifiers,
/// mice via their button codes (BTN_SIDE/BTN_EXTRA are hotkey-relevant).
fn is_capturable(dev: &RawDevice) -> bool {
    if !dev.supported_events().contains(EventType::KEY) {
        return false;
    }
    match dev.supported_keys() {
        Some(keys) => keys
            .iter()
            .any(|code| key_code_to_key(code).is_some() || key_code_to_modifier(code).is_some()),
        None => false,
    }
}

/// Only keyboards are ever grabbed. A pointer (mouse, touchpad — anything
/// with relative or absolute X motion) must never be grabbed: re-injection
/// would add latency to every motion event, and a crash mid-grab is far
/// more disruptive. A device qualifies as a keyboard by emitting at least
/// one mappable non-mouse key.
fn is_keyboard(dev: &RawDevice) -> bool {
    let is_pointer = dev
        .supported_relative_axes()
        .is_some_and(|axes| axes.contains(RelativeAxisCode::REL_X))
        || dev
            .supported_absolute_axes()
            .is_some_and(|axes| axes.contains(AbsoluteAxisCode::ABS_X));
    !is_pointer && has_keyboard_keys(dev)
}

/// Whether the device can emit at least one mappable non-mouse key.
fn has_keyboard_keys(dev: &RawDevice) -> bool {
    dev.supported_keys().is_some_and(|keys| {
        keys.iter().any(|code| {
            key_code_to_modifier(code).is_some()
                || key_code_to_key(code).is_some_and(|key| {
                    !matches!(
                        key,
                        Key::MouseLeft
                            | Key::MouseRight
                            | Key::MouseMiddle
                            | Key::MouseX1
                            | Key::MouseX2
                    )
                })
        })
    })
}

/// Whether this device is a re-injection clone created by a handy-keys
/// blocking listener (this process or another).
fn is_passthrough_clone(dev: &RawDevice) -> bool {
    dev.name()
        .is_some_and(|name| name.starts_with(CLONE_NAME_PREFIX))
}

/// Whether any key is physically held on the device right now (EVIOCGKEY).
fn keys_held(dev: &RawDevice) -> bool {
    dev.get_key_state()
        .map(|keys| keys.iter().next().is_some())
        .unwrap_or(false)
}

/// Finish the exclusive grab of a quiet keyboard: create the uinput clone,
/// flush observed-but-ungrabbed events into detection, grab, and verify
/// nothing went down inside the race window. Degrades to read-only with a
/// warning if the clone can't be built or the grab is refused (e.g. EBUSY
/// from another grabber — that won't heal by retrying).
fn complete_grab(device: &mut OpenDevice, cfg: &mut GrabConfig, state: &mut LinuxState) {
    device.pending_grab = false;

    let (output, nodes) = match build_clone(&device.dev) {
        Ok(built) => built,
        Err(e) => {
            eprintln!(
                "handy-keys: cannot create a re-injection device for {} ({e}); its hotkeys will \
                 be detected but not blocked",
                device.path.display()
            );
            return;
        }
    };
    // Register the clone's nodes before anything can discover them via
    // inotify.
    cfg.own_outputs.extend(nodes.iter().cloned());

    // Everything queued so far also reached the compositor (we are not
    // grabbed yet): process it for hotkey detection and state, forward
    // nothing (`output` is not attached yet, so drain_events won't).
    let _ = drain_events(device, state);

    if let Err(e) = device.dev.grab() {
        eprintln!(
            "handy-keys: cannot grab {} for hotkey blocking ({e}); its hotkeys will be detected \
             but not blocked",
            device.path.display()
        );
        for node in &nodes {
            cfg.own_outputs.remove(node);
        }
        return; // clone dropped here, uinput device destroyed
    }

    // A key that went down inside the drain→grab window reached the
    // compositor from the real device; keeping the grab would strand its
    // release inside the clone and wedge the key. Back out and wait for
    // quiet again. (A full tap inside that window — microseconds — stays
    // queued and is forwarded after attachment; the compositor may see it
    // twice. That residual race is unavoidable without kernel support and
    // is harmless compared to a wedge.)
    if keys_held(&device.dev) {
        let _ = device.dev.ungrab();
        for node in &nodes {
            cfg.own_outputs.remove(node);
        }
        device.pending_grab = true;
        return;
    }

    device.output = Some(output);
    device.output_nodes = nodes;
}

/// Create the uinput clone that carries a grabbed keyboard's non-blocked
/// events onward to the rest of the system. Returns the clone together
/// with its /dev/input node paths (for self-filtering).
fn build_clone(dev: &RawDevice) -> io::Result<(VirtualDevice, Vec<PathBuf>)> {
    let name = format!("{CLONE_NAME_PREFIX}{}", dev.name().unwrap_or("keyboard"));

    let mut builder = VirtualDevice::builder()?
        .name(name.as_str())
        // Clone the vendor/product id so layout-specific quirk handling in
        // compositors follows the real hardware.
        .input_id(dev.input_id());
    if let Some(keys) = dev.supported_keys() {
        builder = builder.with_keys(keys)?;
    }
    if let Some(misc) = dev.misc_properties() {
        // MSC_SCAN accompanies key events on most keyboards; declare it so
        // forwarded batches keep it.
        builder = builder.with_msc(misc)?;
    }
    let mut output = builder.build()?;
    // Blocks briefly until sysfs lists the clone's device node, so the
    // node can be registered for self-filtering before inotify reports it.
    let nodes: Vec<PathBuf> = output
        .enumerate_dev_nodes_blocking()?
        .filter_map(|node| node.ok())
        .collect();
    Ok((output, nodes))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Snapshot which tracked modifier keys are physically held right now,
/// per the kernel's own key state (EVIOCGKEY), across all open devices.
fn physical_modifiers(devices: &[OpenDevice]) -> Modifiers {
    let mut held = Modifiers::empty();
    for device in devices {
        if let Ok(keys) = device.dev.get_key_state() {
            for (code, modifier) in MODIFIER_KEYS {
                if keys.contains(code) {
                    held |= modifier;
                }
            }
        }
    }
    held
}

/// Reconcile tracked modifiers against the kernel's key state.
///
/// Stale modifiers are cleared with synthetic release events so consumers
/// tracking press/release state recover; missed presses are adopted
/// silently and ride along on the next real event.
fn reconcile_modifiers(state: &mut LinuxState, physical: Modifiers) {
    for event in release_events(
        state.listener.current_modifiers,
        stale_modifiers(state.listener.current_modifiers, physical),
    ) {
        state.listener.current_modifiers = event.modifiers;
        let _ = state.listener.event_sender.send(event);
    }
    state.listener.current_modifiers |= physical & RECONCILABLE & !state.listener.current_modifiers;
    // A modifier no longer physically held anywhere can't produce a
    // release event that would need blocking; drop its stale blocked bit
    // (e.g. left by a keyboard unplugged mid-hold) so a later forwarded
    // press on another keyboard is never paired with a blocked release.
    state.blocked_modifiers &= physical;
}

/// Send a synthetic key-up on the listener channel for a key whose real
/// release was lost (device vanished mid-press, or eaten by an overflow),
/// so consumers' press/release state machines recover. Modifier keys are
/// excluded — `reconcile_modifiers` handles those.
fn send_lost_key_release(state: &mut LinuxState, code: KeyCode) {
    if key_code_to_modifier(code).is_some() {
        return;
    }
    let Some(key) = key_code_to_key(code) else {
        return;
    };
    let _ = state.listener.event_sender.send(KeyEvent {
        modifiers: state.listener.current_modifiers,
        key: Some(key),
        is_key_down: false,
        changed_modifier: None,
    });
}

/// Repair per-device state after an overflow resync (`SYN_DROPPED`):
/// synthesize channel releases for tracked keys the kernel says are no
/// longer held, and re-inject releases into the uinput clone for every key
/// it might believe is down — the input core silently drops the ones that
/// are no-ops, so only genuinely stuck keys produce events. Without the
/// clone repair, a key-up lost in the overflow would leave the key held at
/// the compositor forever (the grab means nobody else can release it).
fn repair_after_resync(device: &mut OpenDevice, state: &mut LinuxState) {
    let Ok(real) = device.dev.get_key_state() else {
        return; // device is dying; the removal path cleans up
    };

    let lost: Vec<KeyCode> = device
        .keys_down
        .iter()
        .filter(|code| !real.contains(**code))
        .copied()
        .collect();
    for code in lost {
        device.keys_down.remove(&code);
        device.blocked_keys.remove(&code);
        send_lost_key_release(state, code);
    }

    if let Some(output) = &mut device.output {
        if let Some(supported) = device.dev.supported_keys() {
            let releases: Vec<InputEvent> = supported
                .iter()
                .filter(|code| !real.contains(*code))
                .map(|code| InputEvent::new(EventType::KEY.0, code.0, KEY_RELEASED))
                .collect();
            if !releases.is_empty() {
                if let Err(e) = output.emit(&releases) {
                    eprintln!(
                        "handy-keys: failed to repair re-injected key state for {}: {e}",
                        device.path.display()
                    );
                }
            }
        }
    }
}

/// The poll loop: one pollfd per device plus the inotify watch, re-checking
/// the shutdown flag on every wakeup or timeout.
fn run_loop(
    mut devices: Vec<OpenDevice>,
    inotify: Inotify,
    mut grab: Option<GrabConfig>,
    mut state: LinuxState,
    running: Arc<AtomicBool>,
) {
    // Hotplugged nodes that appeared but could not be opened yet, retried
    // on every wakeup until their deadline (udev grants access moments
    // after creation, not always with a further inotify event).
    let mut retry_paths: Vec<(PathBuf, Instant)> = Vec::new();

    while running.load(Ordering::SeqCst) {
        // Complete deferred grabs at the first quiet moment. Cheap: only
        // devices still pending are checked, normally zero.
        if let Some(cfg) = &mut grab {
            for device in &mut devices {
                if device.pending_grab && !keys_held(&device.dev) {
                    complete_grab(device, cfg, &mut state);
                }
            }
        }

        let mut pollfds = Vec::with_capacity(devices.len() + 1);
        pollfds.push(libc::pollfd {
            fd: inotify.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for device in &devices {
            pollfds.push(libc::pollfd {
                fd: device.dev.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, POLL_TIMEOUT_MS) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                // Transient (signal delivery, kernel allocation pressure):
                // retry, with a nap so a persistent EAGAIN/ENOMEM can't
                // become a busy loop.
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) | Some(libc::ENOMEM) => {
                    thread::sleep(Duration::from_millis(POLL_TIMEOUT_MS as u64));
                    continue;
                }
                // Anything else (EFAULT/EINVAL) is a programming error that
                // won't heal; exiting drops the sender, so receivers see
                // EventLoopNotRunning instead of silently waiting forever.
                _ => {
                    eprintln!("handy-keys: poll on input devices failed: {err}");
                    break;
                }
            }
        }

        let mut needs_reconcile = false;
        let mut dead = Vec::new();
        if rc > 0 {
            // pollfds[1 + i] corresponds to devices[i].
            for (i, pollfd) in pollfds[1..].iter().enumerate() {
                if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    dead.push(i);
                    continue;
                }
                if pollfd.revents & libc::POLLIN == 0 {
                    continue;
                }
                match drain_events(&mut devices[i], &mut state) {
                    Ok(resynced) => needs_reconcile |= resynced,
                    Err(_) => dead.push(i), // ENODEV: device unplugged
                }
            }
        }

        if !dead.is_empty() {
            for i in dead.into_iter().rev() {
                let device = devices.remove(i);
                // Dropping the device destroys its clone; unregister the
                // clone's node paths so real hardware reusing them later
                // is not mistaken for our own output.
                if let Some(cfg) = &mut grab {
                    for node in &device.output_nodes {
                        cfg.own_outputs.remove(node);
                    }
                }
                // A device that vanished mid-press takes its key-ups with
                // it: synthesize channel releases so consumers recover.
                // (The clone needs no repair — destroying a uinput device
                // releases the keys held inside it.)
                for code in &device.keys_down {
                    send_lost_key_release(&mut state, *code);
                }
            }
            needs_reconcile = true;
        }

        // Add new devices only after dead entries are gone: a replugged
        // keyboard typically reuses the same /dev/input/eventN path, and
        // when its removal and reappearance land in the same wakeup the
        // dedup check must not match the stale entry.
        if rc > 0 && pollfds[0].revents & libc::POLLIN != 0 {
            for path in inotify.drain_event_nodes() {
                try_add_device(&mut devices, &path, &mut grab, &mut state, &mut retry_paths);
            }
        }
        if !retry_paths.is_empty() {
            let now = Instant::now();
            let mut still_pending = Vec::new();
            for (path, deadline) in retry_paths.drain(..) {
                match add_device(&mut devices, &path, &mut grab, &mut state) {
                    AddOutcome::Added | AddOutcome::Skipped => {}
                    AddOutcome::Failed(_) if now < deadline => still_pending.push((path, deadline)),
                    AddOutcome::Failed(e) => eprintln!(
                        "handy-keys: giving up on hotplugged device {}: {e}",
                        path.display()
                    ),
                }
            }
            retry_paths = still_pending;
        }

        if needs_reconcile {
            reconcile_modifiers(&mut state, physical_modifiers(&devices));
        }
    }
}

/// Drain everything currently readable from one device, forwarding
/// non-blocked events to its uinput clone if grabbed, and repairing state
/// if an overflow resync completed. Returns whether a resync happened
/// (the caller should then reconcile global modifier state).
fn drain_events(device: &mut OpenDevice, state: &mut LinuxState) -> io::Result<bool> {
    let mut resync_completed = false;
    loop {
        match device.dev.fetch_events() {
            Ok(events) => {
                for event in events {
                    match event.destructure() {
                        EventSummary::Synchronization(_, SynchronizationCode::SYN_DROPPED, _) => {
                            device.resyncing = true;
                        }
                        EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                            if device.resyncing {
                                device.resyncing = false;
                                resync_completed = true;
                            }
                            // Forward the completed batch as one unit; emit()
                            // appends the SYN_REPORT itself.
                            if let Some(output) = &mut device.output {
                                if !device.pending.is_empty() {
                                    if let Err(e) = output.emit(&device.pending) {
                                        eprintln!(
                                            "handy-keys: failed to re-inject events for {}: {e}",
                                            device.path.display()
                                        );
                                    }
                                    device.pending.clear();
                                }
                            }
                        }
                        EventSummary::Key(_, code, value) => {
                            // Track held keys for lost-release recovery. A
                            // resync right after corrects any drift this
                            // picks up from a torn batch.
                            if key_code_to_modifier(code).is_none()
                                && key_code_to_key(code).is_some()
                            {
                                match value {
                                    KEY_PRESSED => {
                                        device.keys_down.insert(code);
                                    }
                                    KEY_RELEASED => {
                                        device.keys_down.remove(&code);
                                    }
                                    _ => {}
                                }
                            }

                            // During a resync the event is unreliable for
                            // state tracking (and for a block decision), but
                            // the user really typed it: forward it.
                            let block = if device.resyncing {
                                false
                            } else {
                                process_key(state, &mut device.blocked_keys, code, value)
                            };
                            if block {
                                // Strip the blocked key's MSC_SCAN
                                // companions, queued just before it in the
                                // batch, so the scancode doesn't leak.
                                while device
                                    .pending
                                    .last()
                                    .is_some_and(|e| e.event_type() == EventType::MISC)
                                {
                                    device.pending.pop();
                                }
                            } else if device.output.is_some() {
                                device.pending.push(event);
                            }
                        }
                        // MSC_SCAN and friends: forward within the batch.
                        _ => {
                            if device.output.is_some() {
                                device.pending.push(event);
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            // A signal during the read syscall is not device death; retry.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    if resync_completed {
        repair_after_resync(device, state);
    }
    Ok(resync_completed)
}

/// Translate one kernel key event into the cross-platform KeyEvent stream.
/// Returns whether a grabbed device must withhold this event from its
/// clone (it matched a blocking hotkey, or completes one that did).
fn process_key(
    state: &mut LinuxState,
    blocked_keys: &mut HashSet<KeyCode>,
    code: KeyCode,
    value: i32,
) -> bool {
    let is_key_down = match value {
        KEY_RELEASED => false,
        // Autorepeat maps to a repeated key-down, matching the other
        // platforms' behavior.
        KEY_PRESSED | KEY_AUTOREPEAT => true,
        _ => return false,
    };

    if let Some(changed_modifier) = key_code_to_modifier(code) {
        if value == KEY_AUTOREPEAT {
            // Repeat of a held modifier changes no state; stays blocked
            // iff its press was.
            return state.blocked_modifiers.contains(changed_modifier);
        }
        let prev_modifiers = state.listener.current_modifiers;
        if is_key_down {
            state.listener.current_modifiers |= changed_modifier;
        } else {
            state.listener.current_modifiers &= !changed_modifier;
        }

        // Only emit when the state actually changed (two keyboards holding
        // the same modifier produce duplicate presses).
        let changed = state.listener.current_modifiers != prev_modifiers;
        if changed {
            let _ = state.listener.event_sender.send(KeyEvent {
                modifiers: state.listener.current_modifiers,
                key: None,
                is_key_down,
                changed_modifier: Some(changed_modifier),
            });
        }

        return if is_key_down {
            if changed {
                let block = state
                    .listener
                    .should_block(state.listener.current_modifiers, None);
                if block {
                    state.blocked_modifiers |= changed_modifier;
                } else {
                    // A forwarded press must never be paired with a blocked
                    // release: clear any stale blocked bit (e.g. left
                    // behind by a keyboard that disappeared mid-hold).
                    state.blocked_modifiers &= !changed_modifier;
                }
                block
            } else {
                // Duplicate press from a second keyboard: follow the
                // original decision, otherwise a blocked modifier-only
                // hotkey would leak through the second keyboard's clone.
                state.blocked_modifiers.contains(changed_modifier)
            }
        } else if changed {
            // Block the release iff the press was blocked, regardless of
            // what the hotkey set says now (see `blocked_modifiers`).
            let block = state.blocked_modifiers.contains(changed_modifier);
            state.blocked_modifiers &= !changed_modifier;
            block
        } else {
            // Duplicate release (the first one already cleared the state);
            // its clone never saw a press, so the kernel drops a forwarded
            // orphan key-up anyway.
            state.blocked_modifiers.contains(changed_modifier)
        };
    }

    let Some(key) = key_code_to_key(code) else {
        return false;
    };

    // The press decides; repeats and the release on THIS device follow it,
    // so the re-injected stream never carries an orphan repeat or key-up.
    let block = match value {
        KEY_PRESSED => {
            let block = state
                .listener
                .should_block(state.listener.current_modifiers, Some(key));
            if block {
                blocked_keys.insert(code);
            } else {
                blocked_keys.remove(&code);
            }
            block
        }
        KEY_AUTOREPEAT => blocked_keys.contains(&code),
        _ => blocked_keys.remove(&code),
    };

    // Only report left/right clicks when modifiers are held, to avoid
    // noise (same rule as the macOS and Windows listeners).
    if matches!(key, Key::MouseLeft | Key::MouseRight)
        && state.listener.current_modifiers.is_empty()
    {
        return block;
    }

    let _ = state.listener.event_sender.send(KeyEvent {
        modifiers: state.listener.current_modifiers,
        key: Some(key),
        is_key_down,
        changed_modifier: None,
    });

    block
}

enum AddOutcome {
    Added,
    /// Already open, our own clone, or not a device we capture.
    Skipped,
    Failed(io::Error),
}

/// Open and register a device node, adopting any modifiers already held on
/// it (same as reconciliation does for missed presses).
fn add_device(
    devices: &mut Vec<OpenDevice>,
    path: &Path,
    grab: &mut Option<GrabConfig>,
    state: &mut LinuxState,
) -> AddOutcome {
    if devices.iter().any(|d| d.path == path) {
        return AddOutcome::Skipped;
    }
    match open_device(path, grab) {
        Ok(Some(device)) => {
            state.listener.current_modifiers |= physical_modifiers(std::slice::from_ref(&device));
            devices.push(device);
            AddOutcome::Added
        }
        Ok(None) => AddOutcome::Skipped,
        Err(e) => AddOutcome::Failed(e),
    }
}

/// Handle a device node reported by inotify. Open failures are expected —
/// inotify fires IN_CREATE before udev has granted read access — so the
/// path is queued for retry on subsequent wakeups until
/// `HOTPLUG_RETRY_WINDOW` elapses, since the access grant does not always
/// produce a further inotify event of its own.
fn try_add_device(
    devices: &mut Vec<OpenDevice>,
    path: &Path,
    grab: &mut Option<GrabConfig>,
    state: &mut LinuxState,
    retry_paths: &mut Vec<(PathBuf, Instant)>,
) {
    match add_device(devices, path, grab, state) {
        AddOutcome::Added | AddOutcome::Skipped => {}
        AddOutcome::Failed(_) => {
            if !retry_paths.iter().any(|(p, _)| p == path) {
                retry_paths.push((path.to_path_buf(), Instant::now() + HOTPLUG_RETRY_WINDOW));
            }
        }
    }
}

/// Minimal inotify wrapper watching one directory for device hotplug.
struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    fn watch(dir: &str) -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let c_dir = CString::new(dir).expect("watch dir contains no NUL");
        // IN_CREATE: a node appeared. IN_ATTRIB: udev adjusted its
        // ownership/mode, which is the moment opening typically first
        // succeeds for non-root users.
        let wd = unsafe {
            libc::inotify_add_watch(
                fd.as_raw_fd(),
                c_dir.as_ptr(),
                libc::IN_CREATE | libc::IN_ATTRIB,
            )
        };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drain pending inotify events, returning the paths of event* nodes
    /// that appeared or changed attributes.
    fn drain_event_nodes(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break; // drained (EAGAIN), or watch gone — nothing to do
            }

            let mut offset = 0usize;
            let header_len = std::mem::size_of::<libc::inotify_event>();
            while offset + header_len <= n as usize {
                // read_unaligned: the byte buffer offers no alignment
                // guarantee for the packed event stream.
                let event = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr().add(offset) as *const libc::inotify_event)
                };
                let name_start = offset + header_len;
                let name_end = name_start + event.len as usize;
                if name_end > n as usize {
                    break; // malformed/truncated entry; drop the rest
                }
                // The name field is NUL-padded to its declared length.
                let name_bytes = &buf[name_start..name_end];
                let name_len = name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(name_bytes.len());
                if let Ok(name) = std::str::from_utf8(&name_bytes[..name_len]) {
                    if name.starts_with("event") {
                        paths.push(Path::new(DEV_INPUT).join(name));
                    }
                }
                offset = name_end;
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Hotkey;
    use std::sync::mpsc::Receiver;
    use std::sync::Mutex;

    fn state_and_receiver_with(
        hotkeys: &[Hotkey],
    ) -> (LinuxState, Receiver<KeyEvent>, BlockingHotkeys) {
        let (tx, rx) = mpsc::channel();
        let blocking: BlockingHotkeys =
            Arc::new(Mutex::new(hotkeys.iter().cloned().collect::<HashSet<_>>()));
        let state = LinuxState {
            listener: ListenerState::new(tx, Some(Arc::clone(&blocking))),
            blocked_modifiers: Modifiers::empty(),
        };
        (state, rx, blocking)
    }

    fn state_and_receiver() -> (LinuxState, Receiver<KeyEvent>) {
        let (state, rx, _) = state_and_receiver_with(&[]);
        (state, rx)
    }

    fn recv_event(rx: &Receiver<KeyEvent>) -> KeyEvent {
        rx.try_recv().expect("expected a key event")
    }

    /// process_key with a throwaway per-device blocked_keys set, for tests
    /// that only exercise one device.
    fn key(state: &mut LinuxState, code: KeyCode, value: i32) -> bool {
        let mut blocked_keys = HashSet::new();
        process_key(state, &mut blocked_keys, code, value)
    }

    #[test]
    fn modifier_events_emit_only_when_state_changes() {
        let (mut state, rx) = state_and_receiver();

        key(&mut state, KeyCode::KEY_LEFTSHIFT, KEY_PRESSED);
        let event = recv_event(&rx);
        assert_eq!(event.modifiers, Modifiers::SHIFT_LEFT);
        assert_eq!(event.key, None);
        assert!(event.is_key_down);
        assert_eq!(event.changed_modifier, Some(Modifiers::SHIFT_LEFT));

        // Duplicate press (second keyboard holding the same key): no event.
        key(&mut state, KeyCode::KEY_LEFTSHIFT, KEY_PRESSED);
        assert!(rx.try_recv().is_err());

        key(&mut state, KeyCode::KEY_LEFTSHIFT, KEY_RELEASED);
        let event = recv_event(&rx);
        assert_eq!(event.modifiers, Modifiers::empty());
        assert!(!event.is_key_down);
        assert_eq!(event.changed_modifier, Some(Modifiers::SHIFT_LEFT));
    }

    #[test]
    fn modifier_autorepeat_is_silent() {
        let (mut state, rx) = state_and_receiver();

        key(&mut state, KeyCode::KEY_LEFTCTRL, KEY_PRESSED);
        let _ = recv_event(&rx);

        key(&mut state, KeyCode::KEY_LEFTCTRL, KEY_AUTOREPEAT);
        assert!(rx.try_recv().is_err());
        assert_eq!(state.listener.current_modifiers, Modifiers::CTRL_LEFT);
    }

    #[test]
    fn autorepeat_is_a_repeated_key_down() {
        let (mut state, rx) = state_and_receiver();

        key(&mut state, KeyCode::KEY_A, KEY_AUTOREPEAT);
        let event = recv_event(&rx);
        assert_eq!(event.key, Some(Key::A));
        assert!(event.is_key_down);
        assert_eq!(event.changed_modifier, None);
    }

    #[test]
    fn keys_carry_current_modifiers() {
        let (mut state, rx) = state_and_receiver();

        key(&mut state, KeyCode::KEY_LEFTMETA, KEY_PRESSED);
        let _ = recv_event(&rx);

        key(&mut state, KeyCode::KEY_F20, KEY_PRESSED);
        let event = recv_event(&rx);
        assert_eq!(event.key, Some(Key::F20));
        assert_eq!(event.modifiers, Modifiers::CMD_LEFT);
        assert!(event.is_key_down);
    }

    #[test]
    fn unmapped_keys_and_values_are_ignored() {
        let (mut state, rx) = state_and_receiver();

        key(&mut state, KeyCode::KEY_POWER, KEY_PRESSED);
        assert!(rx.try_recv().is_err());

        // Unknown event values (nothing beyond 0/1/2 is defined for keys).
        key(&mut state, KeyCode::KEY_A, 3);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn common_mouse_buttons_require_modifiers() {
        let (mut state, rx) = state_and_receiver();

        key(&mut state, KeyCode::BTN_LEFT, KEY_PRESSED);
        assert!(rx.try_recv().is_err());

        key(&mut state, KeyCode::KEY_LEFTSHIFT, KEY_PRESSED);
        let _ = recv_event(&rx);

        key(&mut state, KeyCode::BTN_LEFT, KEY_PRESSED);
        let event = recv_event(&rx);
        assert_eq!(event.key, Some(Key::MouseLeft));
        assert_eq!(event.modifiers, Modifiers::SHIFT_LEFT);
    }

    #[test]
    fn side_mouse_buttons_always_report() {
        let (mut state, rx) = state_and_receiver();

        key(&mut state, KeyCode::BTN_SIDE, KEY_PRESSED);
        let event = recv_event(&rx);
        assert_eq!(event.key, Some(Key::MouseX1));
        assert_eq!(event.modifiers, Modifiers::empty());
    }

    #[test]
    fn reconcile_clears_stale_and_adopts_missed() {
        let (mut state, rx) = state_and_receiver();
        state.listener.current_modifiers = Modifiers::CMD_LEFT | Modifiers::SHIFT_LEFT;

        // Kernel says: Shift still held, Cmd released, Ctrl newly pressed.
        reconcile_modifiers(&mut state, Modifiers::SHIFT_LEFT | Modifiers::CTRL_RIGHT);

        // Stale Cmd cleared with a synthetic release event…
        let event = recv_event(&rx);
        assert_eq!(event.changed_modifier, Some(Modifiers::CMD_LEFT));
        assert!(!event.is_key_down);
        assert_eq!(event.modifiers, Modifiers::SHIFT_LEFT);
        // …missed Ctrl press adopted silently.
        assert!(rx.try_recv().is_err());
        assert_eq!(
            state.listener.current_modifiers,
            Modifiers::SHIFT_LEFT | Modifiers::CTRL_RIGHT
        );
    }

    #[test]
    fn blocked_hotkey_press_repeat_and_release() {
        let hotkey = Hotkey::new(Modifiers::empty(), Key::F20).unwrap();
        let (mut state, rx, _blocking) = state_and_receiver_with(&[hotkey]);
        let mut device_keys = HashSet::new();

        // The whole press-repeat-release cycle of a matching key is blocked…
        assert!(process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_F20,
            KEY_PRESSED
        ));
        assert!(process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_F20,
            KEY_AUTOREPEAT
        ));
        assert!(process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_F20,
            KEY_RELEASED
        ));

        // …but every event still reaches the listener's own channel.
        assert_eq!(rx.try_iter().count(), 3);

        // A non-matching key passes through.
        assert!(!process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_A,
            KEY_PRESSED
        ));
        assert!(!process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_A,
            KEY_RELEASED
        ));
    }

    #[test]
    fn modifiers_gate_blocking() {
        let hotkey = Hotkey::new(Modifiers::CTRL, Key::Space).unwrap();
        let (mut state, _rx, _blocking) = state_and_receiver_with(&[hotkey]);
        let mut device_keys = HashSet::new();

        // Space alone: not blocked.
        assert!(!process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_SPACE,
            KEY_PRESSED
        ));
        assert!(!process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_SPACE,
            KEY_RELEASED
        ));

        // Ctrl+Space: blocked (Ctrl itself passes through — it is not a
        // modifier-only hotkey).
        assert!(!key(&mut state, KeyCode::KEY_LEFTCTRL, KEY_PRESSED));
        assert!(process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_SPACE,
            KEY_PRESSED
        ));
        assert!(process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_SPACE,
            KEY_RELEASED
        ));
        assert!(!key(&mut state, KeyCode::KEY_LEFTCTRL, KEY_RELEASED));
    }

    #[test]
    fn release_follows_press_decision_when_hotkeys_change() {
        let hotkey = Hotkey::new(Modifiers::empty(), Key::F20).unwrap();
        let (mut state, _rx, blocking) = state_and_receiver_with(&[hotkey]);
        let mut device_keys = HashSet::new();

        // Press blocked, hotkey unregistered mid-hold: release stays
        // blocked, so the clone never emits an orphan key-up.
        assert!(process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_F20,
            KEY_PRESSED
        ));
        blocking.lock().unwrap().clear();
        assert!(process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_F20,
            KEY_RELEASED
        ));

        // Press forwarded, hotkey registered mid-hold: release is
        // forwarded too, so the compositor never sees a stuck key.
        assert!(!process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_F20,
            KEY_PRESSED
        ));
        blocking
            .lock()
            .unwrap()
            .insert(Hotkey::new(Modifiers::empty(), Key::F20).unwrap());
        assert!(!process_key(
            &mut state,
            &mut device_keys,
            KeyCode::KEY_F20,
            KEY_RELEASED
        ));
    }

    #[test]
    fn blocked_keys_pair_per_device() {
        // Keyboard A holds plain Space (forwarded); keyboard B presses
        // Ctrl+Space (blocked). A's later release must still be forwarded:
        // withholding it would stick Space at the compositor.
        let hotkey = Hotkey::new(Modifiers::CTRL, Key::Space).unwrap();
        let (mut state, _rx, _blocking) = state_and_receiver_with(&[hotkey]);
        let mut keyboard_a = HashSet::new();
        let mut keyboard_b = HashSet::new();

        assert!(!process_key(
            &mut state,
            &mut keyboard_a,
            KeyCode::KEY_SPACE,
            KEY_PRESSED
        ));
        assert!(!key(&mut state, KeyCode::KEY_LEFTCTRL, KEY_PRESSED));
        assert!(process_key(
            &mut state,
            &mut keyboard_b,
            KeyCode::KEY_SPACE,
            KEY_PRESSED
        ));

        assert!(
            !process_key(
                &mut state,
                &mut keyboard_a,
                KeyCode::KEY_SPACE,
                KEY_RELEASED
            ),
            "keyboard A's forwarded Space press must get a forwarded release"
        );
        assert!(
            process_key(
                &mut state,
                &mut keyboard_b,
                KeyCode::KEY_SPACE,
                KEY_RELEASED
            ),
            "keyboard B's blocked Space press must keep its release blocked"
        );
    }

    #[test]
    fn duplicate_modifier_press_follows_original_block_decision() {
        // Modifier-only hotkey: keyboard A's Cmd press is blocked; keyboard
        // B pressing the same modifier must not leak it to the compositor,
        // and must not clear the blocked bit out from under A's release.
        let hotkey = Hotkey::new(Modifiers::CMD, None).unwrap();
        let (mut state, _rx, _blocking) = state_and_receiver_with(&[hotkey]);

        assert!(key(&mut state, KeyCode::KEY_LEFTMETA, KEY_PRESSED)); // A: blocked
        assert!(
            key(&mut state, KeyCode::KEY_LEFTMETA, KEY_PRESSED),
            "duplicate press from a second keyboard leaked a blocked modifier"
        );
        assert!(
            key(&mut state, KeyCode::KEY_LEFTMETA, KEY_RELEASED),
            "first release must stay blocked"
        );
        // Second keyboard's release: its clone never saw a press, so the
        // forwarded orphan key-up is dropped by the kernel — either
        // decision is safe, it must just not panic or corrupt state.
        key(&mut state, KeyCode::KEY_LEFTMETA, KEY_RELEASED);
        assert_eq!(state.blocked_modifiers, Modifiers::empty());
    }

    #[test]
    fn forwarded_modifier_press_clears_stale_blocked_bit() {
        // A blocked bit can go stale: the keyboard holding the blocked
        // modifier vanishes, so its release event never arrives to clear
        // it. A later press on another keyboard is forwarded (no hotkey
        // registered) — its release must be forwarded too.
        let (mut state, _rx) = state_and_receiver();
        state.blocked_modifiers = Modifiers::CMD_LEFT;

        assert!(!key(&mut state, KeyCode::KEY_LEFTMETA, KEY_PRESSED));
        assert!(!key(&mut state, KeyCode::KEY_LEFTMETA, KEY_RELEASED));
    }

    #[test]
    fn reconcile_prunes_stale_blocked_modifiers() {
        let hotkey = Hotkey::new(Modifiers::CMD, None).unwrap();
        let (mut state, _rx, _blocking) = state_and_receiver_with(&[hotkey]);

        assert!(key(&mut state, KeyCode::KEY_LEFTMETA, KEY_PRESSED));
        assert_eq!(state.blocked_modifiers, Modifiers::CMD_LEFT);

        // The keyboard holding Cmd disappears: nothing physically held.
        reconcile_modifiers(&mut state, Modifiers::empty());
        assert_eq!(state.blocked_modifiers, Modifiers::empty());
    }

    #[test]
    fn lost_key_release_reaches_channel() {
        let (mut state, rx) = state_and_receiver();
        state.listener.current_modifiers = Modifiers::SHIFT_LEFT;

        // A device vanished while F20 was held: consumers need the key-up.
        send_lost_key_release(&mut state, KeyCode::KEY_F20);
        let event = recv_event(&rx);
        assert_eq!(event.key, Some(Key::F20));
        assert!(!event.is_key_down);
        assert_eq!(event.modifiers, Modifiers::SHIFT_LEFT);

        // Modifier codes are reconcile_modifiers' job: no event here.
        send_lost_key_release(&mut state, KeyCode::KEY_LEFTSHIFT);
        assert!(rx.try_recv().is_err());
        // Unmapped codes emit nothing.
        send_lost_key_release(&mut state, KeyCode::KEY_POWER);
        assert!(rx.try_recv().is_err());
    }
}
