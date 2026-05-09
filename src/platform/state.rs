//! Shared state for platform-specific keyboard listeners

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[cfg(not(all(target_os = "linux", feature = "linux-evdev-readonly")))]
use crate::types::Key;
use crate::types::{Hotkey, KeyEvent, Modifiers};

/// Hotkeys that should be blocked when triggered
pub type BlockingHotkeys = Arc<Mutex<HashSet<Hotkey>>>;

/// Internal state shared with platform-specific event callbacks
pub struct ListenerState {
    pub event_sender: Sender<KeyEvent>,
    /// Track which modifiers are currently held
    pub current_modifiers: Modifiers,
    /// Hotkeys to block (if any)
    #[cfg(not(all(target_os = "linux", feature = "linux-evdev-readonly")))]
    pub blocking_hotkeys: Option<BlockingHotkeys>,
}

impl ListenerState {
    #[cfg(all(target_os = "linux", feature = "linux-evdev-readonly"))]
    pub fn new(event_sender: Sender<KeyEvent>) -> Self {
        Self {
            event_sender,
            current_modifiers: Modifiers::empty(),
        }
    }

    #[cfg(not(all(target_os = "linux", feature = "linux-evdev-readonly")))]
    pub fn new(event_sender: Sender<KeyEvent>, blocking_hotkeys: Option<BlockingHotkeys>) -> Self {
        Self {
            event_sender,
            current_modifiers: Modifiers::empty(),
            blocking_hotkeys,
        }
    }

    /// Check if an event matches a blocking hotkey
    #[cfg(not(all(target_os = "linux", feature = "linux-evdev-readonly")))]
    pub fn should_block(&self, modifiers: Modifiers, key: Option<Key>) -> bool {
        if let Some(ref hotkeys) = self.blocking_hotkeys {
            if let Ok(set) = hotkeys.lock() {
                return set
                    .iter()
                    .any(|h| h.modifiers.matches(modifiers) && h.key == key);
            }
        }
        false
    }
}
