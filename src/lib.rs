//! Global keyboard shortcuts library
//!
//! This library provides a cross-platform way to register and listen for global
//! keyboard shortcuts.
//!
//! # Example
//!
//! ```no_run
//! use handy_keys::{HotkeyManager, Hotkey, Modifiers, Key};
//!
//! fn main() -> handy_keys::Result<()> {
//!     let manager = HotkeyManager::new()?;
//!
//!     // Register Cmd+Shift+K using the type-safe constructor
//!     let hotkey = Hotkey::new(Modifiers::CMD | Modifiers::SHIFT, Key::K)?;
//!     let id = manager.register(hotkey)?;
//!
//!     // Or parse from a string (useful for UI/config input)
//!     let hotkey2: Hotkey = "Ctrl+Alt+Space".parse()?;
//!     let id2 = manager.register(hotkey2)?;
//!
//!     println!("Registered hotkeys: {:?}, {:?}", id, id2);
//!
//!     // Wait for hotkey events
//!     while let Ok(event) = manager.recv() {
//!         println!("Hotkey triggered: {:?}", event.id);
//!     }
//!
//!     Ok(())
//! }
//! ```

mod error;
mod listener;
mod manager;
mod platform;
mod types;

pub use error::{Error, Result};
pub use listener::{BlockingHotkeys, KeyboardListener};
pub use manager::HotkeyManager;
pub use types::{Hotkey, HotkeyEvent, HotkeyId, HotkeyState, Key, KeyEvent, Modifiers};

#[cfg(target_os = "macos")]
pub use platform::macos::{check_accessibility, open_accessibility_settings};
