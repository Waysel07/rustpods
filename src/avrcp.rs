//! Stem presses -> your music player.
//!
//! AirPods send presses as AVRCP commands (single = play/pause, double = next,
//! triple = previous). BlueZ only routes them to a player that registered itself
//! through org.bluez.Media1; otherwise they become keyboard media keys that the
//! desktop may or may not pick up. rustpods registers one bridge player and forwards
//! every command to the MPRIS player you are actually using.

use crate::media;
use anyhow::Result;
use std::collections::HashMap;
use zbus::Connection;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

pub const PATH: &str = "/dev/rustpods/player";

pub struct Bridge {
    pub session: Connection,
    pub status: String,
}

impl Bridge {
    async fn forward(&mut self, method: &str) {
        let Some(name) = media::target(&self.session).await else {
            eprintln!("press: {method}, but no media player is running");
            return;
        };
        eprintln!("press: {method} -> {}", name.trim_start_matches("org.mpris.MediaPlayer2."));
        media::call(&self.session, &name, method).await;
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Bridge {
    // AirPods track the state themselves and may send Play when we are playing or
    // Pause when we are paused; one press always means "toggle", so treat both as that.
    async fn play(&mut self) {
        self.forward("PlayPause").await;
    }
    async fn pause(&mut self) {
        self.forward("PlayPause").await;
    }
    async fn play_pause(&mut self) {
        self.forward("PlayPause").await;
    }
    async fn stop(&mut self) {
        self.forward("Stop").await;
    }
    async fn next(&mut self) {
        self.forward("Next").await;
    }
    async fn previous(&mut self) {
        self.forward("Previous").await;
    }
    async fn fast_forward(&mut self) {}
    async fn rewind(&mut self) {}

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.status.clone()
    }
    #[zbus(property)]
    fn position(&self) -> i64 {
        0
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'_>> {
        let mut m = HashMap::new();
        m.insert("xesam:title".to_string(), Value::from("rustpods"));
        m
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

/// Exports the bridge on the system bus and registers it with the adapter.
pub async fn register(system: &Connection, session: &Connection, adapter: &OwnedObjectPath) -> Result<()> {
    let status = current_status(session).await;
    system.object_server().at(PATH, Bridge { session: session.clone(), status: status.clone() }).await?;
    let mut props: HashMap<&str, Value> = HashMap::new();
    props.insert("PlaybackStatus", Value::from(status));
    props.insert("CanGoNext", Value::from(true));
    props.insert("CanGoPrevious", Value::from(true));
    props.insert("CanPlay", Value::from(true));
    props.insert("CanPause", Value::from(true));
    props.insert("CanControl", Value::from(true));
    system
        .call_method(
            Some("org.bluez"),
            adapter.as_str(),
            Some("org.bluez.Media1"),
            "RegisterPlayer",
            &(ObjectPath::try_from(PATH)?, props),
        )
        .await?;
    Ok(())
}

async fn current_status(session: &Connection) -> String {
    match media::target(session).await {
        Some(n) => media::status_of(session, &n).await.unwrap_or_else(|| "Stopped".into()),
        None => "Stopped".into(),
    }
}

/// Keeps the bridge's PlaybackStatus in step with the real player, so the AirPods
/// (and the phone-style state they keep) agree with what is playing.
pub async fn sync_status(system: &Connection, session: &Connection) {
    let Ok(iface) = system.object_server().interface::<_, Bridge>(PATH).await else { return };
    let now = current_status(session).await;
    let mut b = iface.get_mut().await;
    if b.status != now {
        b.status = now;
        let emitter: &SignalEmitter = iface.signal_emitter();
        let _ = b.playback_status_changed(emitter).await;
    }
}
