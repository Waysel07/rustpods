//! Apple Accessory Protocol (AAP) as spoken by AirPods over L2CAP PSM 0x1001.
//!
//! This is the private channel an iPhone or Mac uses to read battery levels and
//! ear detection and to switch noise control. The packet layouts below come from
//! community reverse engineering (LibrePods, OpenPods, MagicPods) and only the
//! well understood parts are used.

use serde::{Deserialize, Serialize};

pub const PSM: u16 = 0x1001;

/// First packet after connecting. Without it the AirPods ignore everything else.
pub const HANDSHAKE: [u8; 16] = [
    0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// Unlocks optional features such as conversational awareness reports.
pub const SET_FEATURES: [u8; 14] = [
    0x04, 0x00, 0x04, 0x00, 0x4d, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// Asks the AirPods to push battery, ear and setting changes from now on.
pub const REQUEST_NOTIFICATIONS: [u8; 10] = [0x04, 0x00, 0x04, 0x00, 0x0f, 0x00, 0xff, 0xff, 0xff, 0xff];

const HEADER: [u8; 4] = [0x04, 0x00, 0x04, 0x00];
const OP_BATTERY: u8 = 0x04;
const OP_EAR: u8 = 0x06;
const OP_CONTROL: u8 = 0x09;
const OP_METADATA: u8 = 0x1d;
const OP_SPEAKING: u8 = 0x4b;

const CTL_NOISE: u8 = 0x0d;
const CTL_CONVERSATION: u8 = 0x28;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Component {
    Single,
    Right,
    Left,
    Case,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Charge {
    Unknown,
    Charging,
    Discharging,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Battery {
    pub component: Component,
    pub level: u8,
    pub charge: Charge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ear {
    In,
    Out,
    Case,
}

impl Ear {
    fn from_byte(b: u8) -> Option<Ear> {
        match b {
            0x00 => Some(Ear::In),
            0x01 => Some(Ear::Out),
            0x02 => Some(Ear::Case),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoiseMode {
    Off,
    Anc,
    Transparency,
    Adaptive,
}

impl NoiseMode {
    fn byte(self) -> u8 {
        match self {
            NoiseMode::Off => 0x01,
            NoiseMode::Anc => 0x02,
            NoiseMode::Transparency => 0x03,
            NoiseMode::Adaptive => 0x04,
        }
    }
    fn from_byte(b: u8) -> Option<NoiseMode> {
        match b {
            0x01 => Some(NoiseMode::Off),
            0x02 => Some(NoiseMode::Anc),
            0x03 => Some(NoiseMode::Transparency),
            0x04 => Some(NoiseMode::Adaptive),
            _ => None,
        }
    }
    pub fn parse(s: &str) -> Option<NoiseMode> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Some(NoiseMode::Off),
            "anc" | "on" | "noise-cancellation" | "nc" => Some(NoiseMode::Anc),
            "transparency" | "trans" | "t" => Some(NoiseMode::Transparency),
            "adaptive" | "a" => Some(NoiseMode::Adaptive),
            _ => None,
        }
    }
}

/// Something the AirPods told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Battery(Vec<Battery>),
    /// Status of the primary and the secondary bud.
    Ear(Ear, Ear),
    Noise(NoiseMode),
    Conversation(bool),
    /// Conversational awareness: the wearer started (true) or stopped (false) talking.
    Speaking(bool),
    Name(String),
    Other(u8),
}

pub fn parse(p: &[u8]) -> Option<Event> {
    if p.len() < 6 || p[..4] != HEADER {
        return None;
    }
    let op = p[4];
    let body = &p[6..];
    match op {
        OP_BATTERY => {
            let n = *body.first()? as usize;
            let mut out = Vec::with_capacity(n);
            for chunk in body[1..].chunks_exact(5).take(n) {
                let component = match chunk[0] {
                    0x01 => Component::Single,
                    0x02 => Component::Right,
                    0x04 => Component::Left,
                    0x08 => Component::Case,
                    _ => continue,
                };
                let charge = match chunk[3] {
                    0x01 => Charge::Charging,
                    0x02 => Charge::Discharging,
                    0x04 => Charge::Disconnected,
                    _ => Charge::Unknown,
                };
                out.push(Battery { component, level: chunk[2].min(100), charge });
            }
            Some(Event::Battery(out))
        }
        OP_EAR if body.len() >= 2 => Some(Event::Ear(Ear::from_byte(body[0])?, Ear::from_byte(body[1])?)),
        OP_CONTROL if body.len() >= 2 => match body[0] {
            CTL_NOISE => NoiseMode::from_byte(body[1]).map(Event::Noise),
            CTL_CONVERSATION => Some(Event::Conversation(body[1] == 0x01)),
            other => Some(Event::Other(other)),
        },
        OP_SPEAKING if !body.is_empty() => {
            // 0x01/0x02: speech started, 0x03+: speech ended / volume restore.
            let b = *body.last()?;
            Some(Event::Speaking(matches!(b, 0x01 | 0x02)))
        }
        OP_METADATA => {
            // Body is a run of NUL separated strings; the first readable one is the name.
            let name = body
                .split(|&b| b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .map(|s| s.trim_matches(|c: char| c.is_control()).trim())
                .find(|s| s.len() >= 3 && s.chars().all(|c| !c.is_control()))?;
            Some(Event::Name(name.to_string()))
        }
        _ => Some(Event::Other(op)),
    }
}

fn control(id: u8, value: u8) -> Vec<u8> {
    vec![0x04, 0x00, 0x04, 0x00, OP_CONTROL, 0x00, id, value, 0x00, 0x00, 0x00]
}

pub fn set_noise(mode: NoiseMode) -> Vec<u8> {
    control(CTL_NOISE, mode.byte())
}

pub fn set_conversation(on: bool) -> Vec<u8> {
    control(CTL_CONVERSATION, if on { 0x01 } else { 0x02 })
}

/// Human readable model name from the product ID in BlueZ's Modalias.
pub fn model_name(product: u16) -> &'static str {
    match product {
        0x2002 => "AirPods (1st gen)",
        0x200f => "AirPods (2nd gen)",
        0x2013 => "AirPods (3rd gen)",
        0x2019 => "AirPods 4",
        0x201b => "AirPods 4 (ANC)",
        0x200e => "AirPods Pro",
        0x2014 => "AirPods Pro 2",
        0x2024 => "AirPods Pro 2 (USB-C)",
        0x2027 => "AirPods Pro 3",
        0x200a => "AirPods Max",
        0x201f => "AirPods Max (USB-C)",
        0x2012 | 0x2017 | 0x2016 | 0x200b | 0x200c | 0x2011 | 0x2010 | 0x2006 | 0x2003 | 0x2005 | 0x2009 => {
            "Beats headphones"
        }
        _ => "Apple audio device",
    }
}

/// Which noise control modes a model supports.
pub fn noise_modes(product: u16) -> &'static [NoiseMode] {
    use NoiseMode::*;
    match product {
        0x2014 | 0x2024 | 0x2027 | 0x201b => &[Off, Anc, Transparency, Adaptive],
        0x200e | 0x200a | 0x201f => &[Off, Anc, Transparency],
        _ => &[],
    }
}

pub fn has_conversation_awareness(product: u16) -> bool {
    matches!(product, 0x2014 | 0x2024 | 0x2027 | 0x201b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_three_components() {
        let p = [
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x03, //
            0x02, 0x01, 0x55, 0x02, 0x01, //
            0x04, 0x01, 0x50, 0x01, 0x01, //
            0x08, 0x01, 0x64, 0x04, 0x01,
        ];
        let Some(Event::Battery(b)) = parse(&p) else { panic!() };
        assert_eq!(b.len(), 3);
        assert_eq!(b[0], Battery { component: Component::Right, level: 85, charge: Charge::Discharging });
        assert_eq!(b[1], Battery { component: Component::Left, level: 80, charge: Charge::Charging });
        assert_eq!(b[2].charge, Charge::Disconnected);
    }

    #[test]
    fn truncated_battery_is_safe() {
        let p = [0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x03, 0x02, 0x01];
        assert_eq!(parse(&p), Some(Event::Battery(vec![])));
    }

    #[test]
    fn ear_detection() {
        let p = [0x04, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00, 0x01];
        assert_eq!(parse(&p), Some(Event::Ear(Ear::In, Ear::Out)));
    }

    #[test]
    fn noise_roundtrip() {
        for m in [NoiseMode::Off, NoiseMode::Anc, NoiseMode::Transparency, NoiseMode::Adaptive] {
            assert_eq!(parse(&set_noise(m)), Some(Event::Noise(m)));
        }
    }

    #[test]
    fn conversation_roundtrip() {
        assert_eq!(parse(&set_conversation(true)), Some(Event::Conversation(true)));
        assert_eq!(parse(&set_conversation(false)), Some(Event::Conversation(false)));
    }

    #[test]
    fn junk_is_ignored() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&HANDSHAKE), None);
        assert_eq!(parse(&[0x04, 0x00, 0x04, 0x00, 0x06, 0x00, 0x09, 0x09]), None);
    }

    #[test]
    fn metadata_name() {
        let mut p = vec![0x04, 0x00, 0x04, 0x00, 0x1d, 0x00, 0x01, 0x00];
        p.extend_from_slice(b"\x00My AirPods\x00A2084\x00");
        assert_eq!(parse(&p), Some(Event::Name("My AirPods".into())));
    }
}
