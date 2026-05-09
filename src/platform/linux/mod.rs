//! Linux platform support.

#[cfg(not(any(feature = "linux-rdev-grab", feature = "linux-evdev-readonly")))]
compile_error!("enable either the linux-rdev-grab or linux-evdev-readonly feature");

#[cfg(all(feature = "linux-rdev-grab", feature = "linux-evdev-readonly"))]
compile_error!("linux-rdev-grab and linux-evdev-readonly are mutually exclusive");

#[cfg(feature = "linux-evdev-readonly")]
#[path = "keycode_evdev.rs"]
pub(crate) mod keycode;

#[cfg(feature = "linux-rdev-grab")]
pub(crate) mod keycode;

#[cfg(feature = "linux-evdev-readonly")]
#[path = "listener_evdev.rs"]
pub(crate) mod listener;

#[cfg(feature = "linux-rdev-grab")]
pub(crate) mod listener;
