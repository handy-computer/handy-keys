//! Shared state for platform-specific keyboard listeners

use std::collections::HashSet;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::types::Hotkey;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::types::Key;
use crate::types::{KeyEvent, Modifiers};

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

/// Canonical order in which synthetic modifier release events are emitted
/// by reconciliation, shared by the Windows and Linux listeners so both
/// backends recover from a stuck modifier with the same event sequence.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub const MODIFIER_RELEASE_ORDER: [Modifiers; 8] = [
    Modifiers::CMD_LEFT,
    Modifiers::CMD_RIGHT,
    Modifiers::SHIFT_LEFT,
    Modifiers::SHIFT_RIGHT,
    Modifiers::CTRL_LEFT,
    Modifiers::CTRL_RIGHT,
    Modifiers::OPT_LEFT,
    Modifiers::OPT_RIGHT,
];

/// Every modifier bit reconciliation may touch. FN is excluded: neither
/// Windows nor Linux evdev ever reports the laptop Fn key (it is handled
/// in firmware), so reconciliation must not clear it.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub const RECONCILABLE: Modifiers = Modifiers::CMD
    .union(Modifiers::SHIFT)
    .union(Modifiers::CTRL)
    .union(Modifiers::OPT);

/// Modifiers we track as held that are no longer physically held.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn stale_modifiers(tracked: Modifiers, physical: Modifiers) -> Modifiers {
    (tracked & RECONCILABLE) & !physical
}

/// Build the synthetic release events that clear `stale` from `tracked`, in
/// MODIFIER_RELEASE_ORDER. Each event carries the modifier set as it
/// shrinks, exactly as if the keys had been released one by one.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn release_events(tracked: Modifiers, stale: Modifiers) -> Vec<KeyEvent> {
    let mut modifiers = tracked;
    let mut events = Vec::new();
    for modifier in MODIFIER_RELEASE_ORDER {
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

#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn stale_modifiers_flags_released_keys() {
        // Tracked Cmd+Ctrl, but only Ctrl still physically held: Cmd is stale.
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
        // FN is not reconcilable: never reported stale.
        let stale = stale_modifiers(Modifiers::FN | Modifiers::CMD_LEFT, Modifiers::empty());
        assert_eq!(stale, Modifiers::CMD_LEFT);
    }

    #[test]
    fn release_events_shrink_modifiers_one_key_at_a_time() {
        let tracked = Modifiers::CMD_LEFT | Modifiers::CTRL_LEFT | Modifiers::FN;
        let stale = Modifiers::CMD_LEFT | Modifiers::CTRL_LEFT;
        let events = release_events(tracked, stale);

        assert_eq!(events.len(), 2);
        // MODIFIER_RELEASE_ORDER: CMD_LEFT before CTRL_LEFT.
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
}
