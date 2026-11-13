//! Pausing/resuming players over MPRIS and ducking volume via PipeWire's wpctl.

use anyhow::Result;
use std::process::Command;
use zbus::Connection;
use zbus::fdo::DBusProxy;

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

async fn players(session: &Connection) -> Result<Vec<String>> {
    let dbus = DBusProxy::new(session).await?;
    Ok(dbus
        .list_names()
        .await?
        .into_iter()
        .map(|n| n.to_string())
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
        .collect())
}

async fn status(session: &Connection, name: &str) -> Option<String> {
    let p = zbus::Proxy::new(session, name.to_string(), MPRIS_PATH, PLAYER).await.ok()?;
    p.get_property::<String>("PlaybackStatus").await.ok()
}

pub async fn status_of(session: &Connection, name: &str) -> Option<String> {
    status(session, name).await
}

/// The player a button press is meant for: the one playing, else a paused one,
/// else whatever MPRIS player exists. Browsers' extra instances count as players too.
pub async fn target(session: &Connection) -> Option<String> {
    let names = players(session).await.ok()?;
    let mut paused = None;
    let mut any = None;
    for n in names {
        match status(session, &n).await.as_deref() {
            Some("Playing") => return Some(n),
            Some("Paused") if paused.is_none() => paused = Some(n),
            _ if any.is_none() => any = Some(n),
            _ => {}
        }
    }
    paused.or(any)
}

pub async fn call(session: &Connection, name: &str, method: &str) {
    let _ = session.call_method(Some(name), MPRIS_PATH, Some(PLAYER), method, &()).await;
}

/// Pauses every playing player and returns their bus names so they can be resumed.
pub async fn pause_all(session: &Connection) -> Result<Vec<String>> {
    let mut paused = vec![];
    for name in players(session).await? {
        if status(session, &name).await.as_deref() == Some("Playing") {
            let ok = session.call_method(Some(name.as_str()), MPRIS_PATH, Some(PLAYER), "Pause", &()).await;
            if ok.is_ok() {
                paused.push(name);
            }
        }
    }
    Ok(paused)
}

/// Resumes the given players, skipping ones the user started or closed meanwhile.
pub async fn resume(session: &Connection, names: &[String]) {
    for name in names {
        if status(session, name).await.as_deref() == Some("Paused") {
            let _ = session.call_method(Some(name.as_str()), MPRIS_PATH, Some(PLAYER), "Play", &()).await;
        }
    }
}

/// Current default sink volume (1.0 = 100 %), if wpctl is available.
pub fn volume() -> Option<f32> {
    let out = Command::new("wpctl").args(["get-volume", "@DEFAULT_AUDIO_SINK@"]).output().ok()?;
    // "Volume: 0.45" or "Volume: 0.45 [MUTED]"
    String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1)?.parse().ok()
}

pub fn set_volume(v: f32) {
    let _ = Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{:.2}", v.clamp(0.0, 1.0))])
        .status();
}

// ---------------------------------------------------------------------------
// Bluetooth card profiles: AirPods stuck in HSP/HFP sound like a 2005 phone call.

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub description: String,
    pub priority: u32,
    pub available: bool,
}

pub fn card_name(address: &str) -> String {
    format!("bluez_card.{}", address.replace(':', "_"))
}

/// Profiles of the card and the active one, parsed from `pactl list cards`.
pub fn profiles(address: &str) -> Option<(Vec<Profile>, String)> {
    let out = Command::new("pactl").args(["list", "cards"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let want = format!("Name: {}", card_name(address));
    let card = text.split("\nCard #").find(|c| c.contains(&want))?;
    let mut list = vec![];
    let mut active = String::new();
    let mut in_profiles = false;
    for line in card.lines() {
        let t = line.trim();
        if t == "Profiles:" {
            in_profiles = true;
            continue;
        }
        if let Some(a) = t.strip_prefix("Active Profile:") {
            active = a.trim().to_string();
            in_profiles = false;
            continue;
        }
        if in_profiles {
            // "a2dp-sink: High Fidelity Playback (...) (sinks: 1, sources: 0, priority: 133, available: yes)"
            let Some((name, rest)) = t.split_once(": ") else {
                in_profiles = false;
                continue;
            };
            let priority = rest
                .split("priority: ")
                .nth(1)
                .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let description = rest.split(" (sinks").next().unwrap_or(rest).to_string();
            list.push(Profile {
                name: name.to_string(),
                description,
                priority,
                available: !rest.contains("available: no"),
            });
        }
    }
    Some((list, active))
}

/// Best profile of a kind: "a2dp" for music, "headset" for calls with the mic.
pub fn best_profile(list: &[Profile], prefix: &str) -> Option<Profile> {
    list.iter()
        .filter(|p| p.available && p.name.starts_with(prefix))
        .max_by_key(|p| p.priority)
        .cloned()
}

pub fn set_profile(address: &str, profile: &str) -> Result<()> {
    let st = Command::new("pactl").args(["set-card-profile", &card_name(address), profile]).status()?;
    anyhow::ensure!(st.success(), "pactl set-card-profile failed");
    Ok(())
}
