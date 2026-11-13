//! User settings in ~/.config/rustpods/config.json, shared by the daemon and the settings window.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a stem press does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum Action {
    /// Leave it to the AirPods (and the normal Bluetooth media commands).
    Default,
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    Mute,
    /// Cycle noise control (off → ANC → transparency → …).
    NoiseNext,
    ConversationToggle,
    ShowPopup,
    /// Run a shell command.
    Command(String),
    Nothing,
}

impl Action {
    pub const CHOICES: [Action; 11] = [
        Action::Default,
        Action::PlayPause,
        Action::Next,
        Action::Previous,
        Action::VolumeUp,
        Action::VolumeDown,
        Action::Mute,
        Action::NoiseNext,
        Action::ConversationToggle,
        Action::ShowPopup,
        Action::Nothing,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Action::Default => "Default",
            Action::PlayPause => "Play / pause",
            Action::Next => "Next song",
            Action::Previous => "Previous song",
            Action::VolumeUp => "Volume up",
            Action::VolumeDown => "Volume down",
            Action::Mute => "Mute",
            Action::NoiseNext => "Next noise mode",
            Action::ConversationToggle => "Conversation awareness on/off",
            Action::ShowPopup => "Show battery popup",
            Action::Command(_) => "Run command…",
            Action::Nothing => "Nothing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Press {
    Single,
    Double,
    Triple,
    Long,
}

impl Press {
    pub const ALL: [Press; 4] = [Press::Single, Press::Double, Press::Triple, Press::Long];
    pub fn label(self) -> &'static str {
        match self {
            Press::Single => "Press once",
            Press::Double => "Press twice",
            Press::Triple => "Press three times",
            Press::Long => "Press and hold",
        }
    }
    /// What the AirPods do on their own.
    pub fn default_label(self) -> &'static str {
        match self {
            Press::Single => "play / pause",
            Press::Double => "next song",
            Press::Triple => "previous song",
            Press::Long => "Siri or noise control",
        }
    }
    /// Bit in the AirPods' "send this press to the host" mask.
    pub fn bit(self) -> u8 {
        match self {
            Press::Single => 0x01,
            Press::Double => 0x02,
            Press::Triple => 0x04,
            Press::Long => 0x08,
        }
    }
    /// Our action that matches what the AirPods would do by default (for the side that
    /// keeps "Default" while the other side is customised).
    pub fn default_action(self) -> Action {
        match self {
            Press::Single => Action::PlayPause,
            Press::Double => Action::Next,
            Press::Triple => Action::Previous,
            Press::Long => Action::NoiseNext,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Side {
    pub single: Action,
    pub double: Action,
    pub triple: Action,
    pub long: Action,
}

impl Default for Side {
    fn default() -> Self {
        Side { single: Action::Default, double: Action::Default, triple: Action::Default, long: Action::Default }
    }
}

impl Side {
    pub fn get(&self, p: Press) -> &Action {
        match p {
            Press::Single => &self.single,
            Press::Double => &self.double,
            Press::Triple => &self.triple,
            Press::Long => &self.long,
        }
    }
    pub fn get_mut(&mut self, p: Press) -> &mut Action {
        match p {
            Press::Single => &mut self.single,
            Press::Double => &mut self.double,
            Press::Triple => &mut self.triple,
            Press::Long => &mut self.long,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ear_pause: bool,
    pub popup: bool,
    pub notify_low: bool,
    pub battery_provider: bool,
    pub media_keys: bool,
    pub duck: bool,
    /// Volume while you talk, as a fraction of the normal volume.
    pub duck_level: f32,
    /// Offer to connect when a paired AirPods case is opened nearby.
    pub connect_assistant: bool,
    pub left: Side,
    pub right: Side,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ear_pause: true,
            popup: true,
            notify_low: true,
            battery_provider: true,
            media_keys: true,
            duck: false,
            duck_level: 0.3,
            connect_assistant: true,
            left: Side::default(),
            right: Side::default(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_default()
            .join("rustpods/config.json")
    }

    /// Missing or broken file → defaults (a broken file is reported, not overwritten).
    pub fn load() -> Config {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("{}: {e}; using defaults", Self::path().display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = Self::path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn side(&self, left: bool) -> &Side {
        if left { &self.left } else { &self.right }
    }

    /// Which presses the AirPods must report to us instead of handling themselves:
    /// every press that is customised on at least one side.
    pub fn capture_mask(&self) -> u8 {
        Press::ALL
            .iter()
            .filter(|p| *self.left.get(**p) != Action::Default || *self.right.get(**p) != Action::Default)
            .fold(0, |m, p| m | p.bit())
    }

    /// The action for a press reported by the AirPods.
    pub fn action(&self, left: bool, p: Press) -> Action {
        match self.side(left).get(p) {
            Action::Default => p.default_action(),
            a => a.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_only_for_customised_presses() {
        let mut c = Config::default();
        assert_eq!(c.capture_mask(), 0);
        c.left.double = Action::VolumeUp;
        assert_eq!(c.capture_mask(), Press::Double.bit());
        c.right.long = Action::Mute;
        assert_eq!(c.capture_mask(), Press::Double.bit() | Press::Long.bit());
    }

    #[test]
    fn default_side_keeps_normal_behaviour() {
        let mut c = Config::default();
        c.left.double = Action::Previous;
        assert_eq!(c.action(true, Press::Double), Action::Previous);
        assert_eq!(c.action(false, Press::Double), Action::Next);
    }

    #[test]
    fn json_roundtrip_and_partial_files() {
        let mut c = Config::default();
        c.right.triple = Action::Command("notify-send hi".into());
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&j).unwrap(), c);
        let partial: Config = serde_json::from_str(r#"{"popup": false}"#).unwrap();
        assert!(!partial.popup && partial.ear_pause);
    }
}
