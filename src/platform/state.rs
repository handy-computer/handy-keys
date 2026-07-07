//! Shared state for platform-specific keyboard listeners

use std::collections::HashSet;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::types::Hotkey;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::types::{Key, KeyEvent, Modifiers};

/// Hotkeys that should be blocked when triggered
pub type BlockingHotkeys = Arc<Mutex<HashSet<Hotkey>>>;

/// Internal state shared with platform-specific event callbacks
///
/// Used by the macOS and Linux backends; the Windows backend keeps its
/// state in a thread-local `HookContext` instead.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub struct ListenerState {
    pub event_sender: Sender<KeyEvent>,
    /// Track which modifiers are currently held
    pub current_modifiers: Modifiers,
    /// Hotkeys to block (if any)
    pub blocking_hotkeys: Option<BlockingHotkeys>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ListenerState {
    pub fn new(event_sender: Sender<KeyEvent>, blocking_hotkeys: Option<BlockingHotkeys>) -> Self {
        Self {
            event_sender,
            current_modifiers: Modifiers::empty(),
            blocking_hotkeys,
        }
    }

    /// Check if an event matches a blocking hotkey
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
