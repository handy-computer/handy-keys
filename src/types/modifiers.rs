//! Modifier key definitions and parsing

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

bitflags! {
    /// Modifier keys for hotkey combinations
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct Modifiers: u32 {
        /// Command key (macOS) / Windows key (Windows) / Super key (Linux)
        const CMD = 1 << 0;
        /// Shift key
        const SHIFT = 1 << 1;
        /// Control key
        const CTRL = 1 << 2;
        /// Option key (macOS) / Alt key (Windows/Linux)
        const OPT = 1 << 3;
        /// Function key (macOS)
        const FN = 1 << 4;
    }
}

impl fmt::Display for Modifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.contains(Modifiers::CTRL) {
            parts.push("Ctrl");
        }
        if self.contains(Modifiers::OPT) {
            parts.push("Opt");
        }
        if self.contains(Modifiers::SHIFT) {
            parts.push("Shift");
        }
        if self.contains(Modifiers::CMD) {
            parts.push("Cmd");
        }
        if self.contains(Modifiers::FN) {
            parts.push("Fn");
        }
        write!(f, "{}", parts.join("+"))
    }
}

impl Modifiers {
    /// Parse a single modifier name (case-insensitive)
    pub(crate) fn parse_single(s: &str) -> Option<Modifiers> {
        match s.to_lowercase().as_str() {
            "cmd" | "command" | "meta" | "super" | "win" | "windows" => Some(Modifiers::CMD),
            "shift" => Some(Modifiers::SHIFT),
            "ctrl" | "control" => Some(Modifiers::CTRL),
            "opt" | "option" | "alt" => Some(Modifiers::OPT),
            "fn" | "function" => Some(Modifiers::FN),
            _ => None,
        }
    }

}

impl FromStr for Modifiers {
    type Err = Error;

    /// Parse modifiers from a string like "Cmd+Shift" or "Ctrl+Alt"
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Modifiers::empty());
        }

        let mut modifiers = Modifiers::empty();
        for part in s.split('+') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match Modifiers::parse_single(part) {
                Some(m) => modifiers |= m,
                None => return Err(Error::UnknownModifier(part.to_string())),
            }
        }
        Ok(modifiers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_modifiers() {
        assert_eq!("Cmd".parse::<Modifiers>().unwrap(), Modifiers::CMD);
        assert_eq!("command".parse::<Modifiers>().unwrap(), Modifiers::CMD);
        assert_eq!("meta".parse::<Modifiers>().unwrap(), Modifiers::CMD);
        assert_eq!("super".parse::<Modifiers>().unwrap(), Modifiers::CMD);
        assert_eq!("win".parse::<Modifiers>().unwrap(), Modifiers::CMD);

        assert_eq!("Shift".parse::<Modifiers>().unwrap(), Modifiers::SHIFT);
        assert_eq!("SHIFT".parse::<Modifiers>().unwrap(), Modifiers::SHIFT);

        assert_eq!("Ctrl".parse::<Modifiers>().unwrap(), Modifiers::CTRL);
        assert_eq!("control".parse::<Modifiers>().unwrap(), Modifiers::CTRL);

        assert_eq!("Opt".parse::<Modifiers>().unwrap(), Modifiers::OPT);
        assert_eq!("option".parse::<Modifiers>().unwrap(), Modifiers::OPT);
        assert_eq!("alt".parse::<Modifiers>().unwrap(), Modifiers::OPT);

        assert_eq!("Fn".parse::<Modifiers>().unwrap(), Modifiers::FN);
        assert_eq!("function".parse::<Modifiers>().unwrap(), Modifiers::FN);
    }

    #[test]
    fn parse_combined_modifiers() {
        assert_eq!(
            "Cmd+Shift".parse::<Modifiers>().unwrap(),
            Modifiers::CMD | Modifiers::SHIFT
        );
        assert_eq!(
            "Ctrl+Alt+Shift".parse::<Modifiers>().unwrap(),
            Modifiers::CTRL | Modifiers::OPT | Modifiers::SHIFT
        );
    }

    #[test]
    fn parse_empty_modifiers() {
        assert_eq!("".parse::<Modifiers>().unwrap(), Modifiers::empty());
        assert_eq!("  ".parse::<Modifiers>().unwrap(), Modifiers::empty());
    }

    #[test]
    fn parse_unknown_modifier_fails() {
        assert!("Unknown".parse::<Modifiers>().is_err());
        assert!("Cmd+Unknown".parse::<Modifiers>().is_err());
    }

    #[test]
    fn modifiers_display() {
        assert_eq!(format!("{}", Modifiers::CMD), "Cmd");
        assert_eq!(format!("{}", Modifiers::SHIFT), "Shift");
        assert_eq!(format!("{}", Modifiers::CMD | Modifiers::SHIFT), "Shift+Cmd");
    }
}
