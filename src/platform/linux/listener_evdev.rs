//! Linux keyboard listener using read-only evdev device streams.
//!
//! This backend does not grab devices. It observes global key events, but it
//! cannot block registered hotkeys from reaching other applications.
//! Devices are discovered once at listener startup.

use std::fs::{read_dir, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use evdev_rs::enums::{EventCode, EV_KEY};
use evdev_rs::{Device, InputEvent, ReadFlag};

use crate::error::{Error, Result};
use crate::platform::state::{BlockingHotkeys, ListenerState};
use crate::types::{Key, KeyEvent, Modifiers};

use super::keycode::{evdev_key_to_key, evdev_key_to_modifier, update_modifiers};

const DEV_INPUT: &str = "/dev/input";

/// Internal listener state returned to KeyboardListener.
pub(crate) struct LinuxListenerState {
    pub event_receiver: Receiver<KeyEvent>,
    pub thread_handle: Option<JoinHandle<()>>,
    pub running: Arc<AtomicBool>,
    pub blocking_hotkeys: Option<BlockingHotkeys>,
}

/// Spawn a read-only evdev keyboard listener for Linux.
pub(crate) fn spawn(blocking_hotkeys: Option<BlockingHotkeys>) -> Result<LinuxListenerState> {
    if blocking_hotkeys.is_some() {
        eprintln!("handy-keys: linux-evdev-readonly observes hotkeys but does not block them");
    }

    let files = get_device_files(DEV_INPUT)?;
    if files.is_empty() {
        return Err(Error::Platform(
            "no readable /dev/input/event* devices found; evdev requires input device read access"
                .into(),
        ));
    }

    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(ListenerState::new(tx)));
    let running = Arc::new(AtomicBool::new(true));

    let thread_state = Arc::clone(&state);
    let thread_running = Arc::clone(&running);

    let handle = thread::spawn(move || {
        for file in files {
            spawn_device_reader(file, Arc::clone(&thread_state), Arc::clone(&thread_running));
        }

        while thread_running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
    });

    Ok(LinuxListenerState {
        event_receiver: rx,
        thread_handle: Some(handle),
        running,
        blocking_hotkeys,
    })
}

fn spawn_device_reader(file: File, state: Arc<Mutex<ListenerState>>, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        let device = match Device::new_from_fd(file) {
            Ok(device) => device,
            Err(error) => {
                eprintln!("handy-keys: skipping unreadable input device: {error}");
                return;
            }
        };

        while running.load(Ordering::SeqCst) {
            match device.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING) {
                Ok((_, event)) => handle_input_event(&event, &state),
                Err(error) => {
                    if running.load(Ordering::SeqCst) {
                        eprintln!("handy-keys: input device reader stopped: {error}");
                    }
                    break;
                }
            }
        }
    });
}

fn get_device_files<T>(path: T) -> std::io::Result<Vec<File>>
where
    T: AsRef<Path>,
{
    let mut files = Vec::new();

    for entry in read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if !file_name.starts_with("event") {
            continue;
        }

        match File::open(&path) {
            Ok(file) => files.push(file),
            Err(error) => eprintln!(
                "handy-keys: skipping input device {}: {error}",
                path.display()
            ),
        }
    }

    Ok(files)
}

fn handle_input_event(event: &InputEvent, state: &Arc<Mutex<ListenerState>>) {
    let EventCode::EV_KEY(ref ev_key) = event.event_code else {
        return;
    };

    let is_key_down = match event.value {
        0 => false,
        1 | 2 => true,
        _ => return,
    };

    if let Ok(mut state) = state.lock() {
        process_key_event(&mut state, ev_key, is_key_down);
    }
}

fn process_key_event(state: &mut ListenerState, ev_key: &EV_KEY, is_key_down: bool) {
    if let Some(changed_modifier) = evdev_key_to_modifier(ev_key) {
        let previous_modifiers = state.current_modifiers;
        state.current_modifiers = update_modifiers(state.current_modifiers, ev_key, is_key_down);

        if state.current_modifiers != previous_modifiers {
            let _ = state.event_sender.send(KeyEvent {
                modifiers: state.current_modifiers,
                key: None,
                is_key_down,
                changed_modifier: Some(changed_modifier),
            });
        }

        return;
    }

    let Some(key) = evdev_key_to_key(ev_key) else {
        return;
    };

    if should_ignore_common_mouse_button(key, state.current_modifiers) {
        return;
    }

    let _ = state.event_sender.send(KeyEvent {
        modifiers: state.current_modifiers,
        key: Some(key),
        is_key_down,
        changed_modifier: None,
    });
}

fn should_ignore_common_mouse_button(key: Key, modifiers: Modifiers) -> bool {
    matches!(key, Key::MouseLeft | Key::MouseRight) && modifiers.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev_rs::TimeVal;
    use std::sync::mpsc::{self, Receiver};

    fn state_and_receiver() -> (ListenerState, Receiver<KeyEvent>) {
        let (tx, rx) = mpsc::channel();
        (ListenerState::new(tx), rx)
    }

    fn recv_event(rx: &Receiver<KeyEvent>) -> KeyEvent {
        rx.try_recv().expect("expected a key event")
    }

    fn input_event(key: EV_KEY, value: i32) -> InputEvent {
        InputEvent::new(&TimeVal::new(0, 0), &EventCode::EV_KEY(key), value)
    }

    #[test]
    fn modifier_events_emit_only_when_state_changes() {
        let (mut state, rx) = state_and_receiver();

        process_key_event(&mut state, &EV_KEY::KEY_LEFTSHIFT, true);
        let event = recv_event(&rx);
        assert_eq!(event.modifiers, Modifiers::SHIFT_LEFT);
        assert_eq!(event.key, None);
        assert!(event.is_key_down);
        assert_eq!(event.changed_modifier, Some(Modifiers::SHIFT_LEFT));

        process_key_event(&mut state, &EV_KEY::KEY_LEFTSHIFT, true);
        assert!(rx.try_recv().is_err());

        process_key_event(&mut state, &EV_KEY::KEY_LEFTSHIFT, false);
        let event = recv_event(&rx);
        assert_eq!(event.modifiers, Modifiers::empty());
        assert_eq!(event.key, None);
        assert!(!event.is_key_down);
        assert_eq!(event.changed_modifier, Some(Modifiers::SHIFT_LEFT));
    }

    #[test]
    fn repeat_events_are_key_down_events() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(ListenerState::new(tx)));

        handle_input_event(&input_event(EV_KEY::KEY_A, 2), &state);

        let event = recv_event(&rx);
        assert_eq!(event.modifiers, Modifiers::empty());
        assert_eq!(event.key, Some(Key::A));
        assert!(event.is_key_down);
        assert_eq!(event.changed_modifier, None);
    }

    #[test]
    fn common_mouse_buttons_require_modifiers() {
        let (mut state, rx) = state_and_receiver();

        process_key_event(&mut state, &EV_KEY::BTN_LEFT, true);
        assert!(rx.try_recv().is_err());

        process_key_event(&mut state, &EV_KEY::KEY_LEFTSHIFT, true);
        let _ = recv_event(&rx);

        process_key_event(&mut state, &EV_KEY::BTN_LEFT, true);
        let event = recv_event(&rx);
        assert_eq!(event.modifiers, Modifiers::SHIFT_LEFT);
        assert_eq!(event.key, Some(Key::MouseLeft));
        assert!(event.is_key_down);
        assert_eq!(event.changed_modifier, None);
    }
}
