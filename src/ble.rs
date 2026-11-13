//! Connection assistant: notice an AirPods case opening nearby and offer to connect.
//!
//! With the lid open, AirPods broadcast an Apple "proximity pairing" BLE advertisement
//! (manufacturer 0x004C, type 0x07) with the model, battery levels and a lid counter.
//! While none of our AirPods are connected, rustpods asks BlueZ for an LE scan and
//! watches those advertisements. Layout as documented by OpenPods.

use crate::aap::{Battery, Charge, Component};
use crate::bluez::{self, Device};
use crate::config::Config;
use crate::popup::PopupData;
use anyhow::Result;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

const APPLE: u16 = 0x004c;
const PROXIMITY_PAIRING: u8 = 0x07;
/// Ignore cases further away than roughly the next room.
const MIN_RSSI: i16 = -72;
/// Silence after which a new advertisement counts as "the lid was just opened".
const QUIET: Duration = Duration::from_secs(20);
/// Don't nag: at most one assistant per pair per this long.
const COOLDOWN: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq)]
pub struct Advert {
    pub model: u16,
    pub batteries: Vec<Battery>,
    pub lid_counter: u8,
}

fn level(nibble: u8) -> Option<u8> {
    (nibble <= 10).then_some(nibble * 10)
}

/// Parses the bytes BlueZ reports under manufacturer 0x004C.
pub fn parse(d: &[u8]) -> Option<Advert> {
    if d.len() < 9 || d[0] != PROXIMITY_PAIRING {
        return None;
    }
    let model = u16::from_le_bytes([d[3], d[4]]);
    let status = d[5];
    // "Flipped" swaps which nibble belongs to which bud.
    let flip = status & 0x20 == 0;
    let (hi, lo) = (d[6] >> 4, d[6] & 0x0f);
    let (left, right) = if flip { (hi, lo) } else { (lo, hi) };
    let charge = d[7] >> 4;
    let case = d[7] & 0x0f;
    let (cl, cr) = if flip { (charge & 0x02 != 0, charge & 0x01 != 0) } else { (charge & 0x01 != 0, charge & 0x02 != 0) };
    let state = |charging: bool| if charging { Charge::Charging } else { Charge::Discharging };
    let mut batteries = vec![];
    if crate::aap::noise_modes(model).len() == 3 && matches!(model, 0x200a | 0x201f) {
        // AirPods Max: one battery.
        if let Some(l) = level(lo) {
            batteries.push(Battery { component: Component::Single, level: l, charge: state(cl || cr) });
        }
    } else {
        if let Some(l) = level(left) {
            batteries.push(Battery { component: Component::Left, level: l, charge: state(cl) });
        }
        if let Some(l) = level(right) {
            batteries.push(Battery { component: Component::Right, level: l, charge: state(cr) });
        }
        if let Some(l) = level(case) {
            batteries.push(Battery { component: Component::Case, level: l, charge: state(charge & 0x04 != 0) });
        }
    }
    Some(Advert { model, batteries, lid_counter: d[8] })
}

/// When the assistant last showed, so the daemon can skip its own popup right after.
pub static LAST_SHOWN: Mutex<Option<Instant>> = Mutex::new(None);

/// True if the assistant popup showed within `d`.
pub fn shown_within(d: Duration) -> bool {
    LAST_SHOWN.lock().unwrap().is_some_and(|t| t.elapsed() < d)
}

struct Seen {
    at: Instant,
    lid: u8,
}

fn apple_data(props: &HashMap<String, OwnedValue>) -> Option<Vec<u8>> {
    let md: HashMap<u16, OwnedValue> = props.get("ManufacturerData")?.try_clone().ok()?.try_into().ok()?;
    let v = md.get(&APPLE)?.try_clone().ok()?;
    Vec::<u8>::try_from(v).ok()
}

async fn rssi(system: &Connection, path: &OwnedObjectPath) -> Option<i16> {
    let p = zbus::Proxy::new(system, "org.bluez", path.as_str(), "org.bluez.Device1").await.ok()?;
    p.get_property::<i16>("RSSI").await.ok()
}

async fn set_scanning(system: &Connection, adapter: &OwnedObjectPath, on: bool) {
    if on {
        let mut filter: HashMap<&str, Value> = HashMap::new();
        filter.insert("Transport", Value::from("le"));
        filter.insert("DuplicateData", Value::from(true));
        let _ = system
            .call_method(Some("org.bluez"), adapter.as_str(), Some("org.bluez.Adapter1"), "SetDiscoveryFilter", &(filter,))
            .await;
        let _ = system.call_method(Some("org.bluez"), adapter.as_str(), Some("org.bluez.Adapter1"), "StartDiscovery", &()).await;
    } else {
        let _ = system.call_method(Some("org.bluez"), adapter.as_str(), Some("org.bluez.Adapter1"), "StopDiscovery", &()).await;
    }
}

/// Runs for the daemon's lifetime. Scans only while no paired AirPods are connected.
pub async fn run(system: Connection, want: Option<String>) -> Result<()> {
    let changed = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .build();
    let added = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")?
        .interface("org.freedesktop.DBus.ObjectManager")?
        .member("InterfacesAdded")?
        .build();
    let mut changes = MessageStream::for_match_rule(changed, &system, Some(64)).await?;
    let mut adds = MessageStream::for_match_rule(added, &system, Some(64)).await?;

    let mut paired: Vec<Device> = vec![];
    let mut scanning: Option<OwnedObjectPath> = None;
    let mut seen: HashMap<u16, Seen> = HashMap::new();
    let mut shown: HashMap<String, Instant> = HashMap::new();
    let mut check = tokio::time::interval(Duration::from_secs(5));

    loop {
        // (object path, changed properties) from either signal kind.
        let (path, props): (OwnedObjectPath, HashMap<String, OwnedValue>) = tokio::select! {
            _ = check.tick() => {
                let enabled = Config::load().connect_assistant;
                paired = bluez::airpods(&system).await.unwrap_or_default();
                if let Some(w) = &want {
                    let wl = w.to_lowercase();
                    paired.retain(|d| d.address.eq_ignore_ascii_case(w) || d.name.to_lowercase().contains(&wl));
                }
                let should = enabled && !paired.is_empty() && !paired.iter().any(|d| d.connected);
                match (&scanning, should) {
                    (None, true) => {
                        let adapter = paired[0].adapter.clone();
                        set_scanning(&system, &adapter, true).await;
                        scanning = Some(adapter);
                    }
                    (Some(a), false) => {
                        set_scanning(&system, a, false).await;
                        scanning = None;
                    }
                    _ => {}
                }
                continue;
            }
            Some(Ok(m)) = changes.next() => {
                let Some(path) = m.header().path().map(|p| OwnedObjectPath::from(p.to_owned())) else { continue };
                let Ok((iface, props, _)) = m.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>() else { continue };
                if iface != "org.bluez.Device1" { continue }
                (path, props)
            }
            Some(Ok(m)) = adds.next() => {
                let Ok((path, mut ifaces)) = m.body().deserialize::<(OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>)>() else { continue };
                let Some(props) = ifaces.remove("org.bluez.Device1") else { continue };
                (path, props)
            }
        };
        if scanning.is_none() {
            continue;
        }
        let Some(data) = apple_data(&props) else { continue };
        let Some(ad) = parse(&data) else { continue };
        // Match the broadcast (random address) to a paired pair by model.
        let Some(dev) = paired.iter().find(|d| d.product == ad.model && !d.connected).cloned() else { continue };

        let now = Instant::now();
        let fresh = match seen.get(&ad.model) {
            None => true,
            Some(s) => now.duration_since(s.at) > QUIET || s.lid != ad.lid_counter,
        };
        seen.insert(ad.model, Seen { at: now, lid: ad.lid_counter });
        if !fresh || shown.get(&dev.address).is_some_and(|t| now.duration_since(*t) < COOLDOWN) {
            continue;
        }
        let signal = match props.get("RSSI").and_then(|v| i16::try_from(v.try_clone().ok()?).ok()) {
            Some(r) => Some(r),
            None => rssi(&system, &path).await,
        };
        if signal.is_some_and(|r| r < MIN_RSSI) {
            continue;
        }
        shown.insert(dev.address.clone(), now);
        *LAST_SHOWN.lock().unwrap() = Some(now);
        eprintln!("case opened nearby: {} (rssi {:?}), offering to connect", dev.name, signal);
        crate::daemon::spawn_popup(&PopupData {
            name: dev.name.clone(),
            model: crate::aap::model_name(dev.product).into(),
            product: dev.product,
            batteries: ad.batteries,
            connect: Some(dev.address.clone()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 07 19 01 | model 13 20 | status | batt L/R | charge+case | lid | ...
    fn advert(status: u8, pods: u8, charge_case: u8) -> Vec<u8> {
        vec![0x07, 0x19, 0x01, 0x13, 0x20, status, pods, charge_case, 0x05, 0x00]
    }

    #[test]
    fn model_and_levels() {
        // flip (status bit 0x20 clear): high nibble = left
        let a = parse(&advert(0x00, 0x86, 0x45)).unwrap();
        assert_eq!(a.model, 0x2013);
        assert_eq!(a.lid_counter, 0x05);
        assert_eq!(a.batteries[0], Battery { component: Component::Left, level: 80, charge: Charge::Discharging });
        assert_eq!(a.batteries[1], Battery { component: Component::Right, level: 60, charge: Charge::Discharging });
        assert_eq!(a.batteries[2], Battery { component: Component::Case, level: 50, charge: Charge::Charging });
    }

    #[test]
    fn not_flipped_swaps_sides() {
        let a = parse(&advert(0x20, 0x86, 0x1a)).unwrap();
        assert_eq!(a.batteries[0], Battery { component: Component::Left, level: 60, charge: Charge::Charging });
        assert_eq!(a.batteries[1], Battery { component: Component::Right, level: 80, charge: Charge::Discharging });
        assert_eq!(a.batteries[2].level, 100);
    }

    #[test]
    fn unknown_levels_are_dropped_and_junk_ignored() {
        let a = parse(&advert(0x00, 0xf6, 0x0f)).unwrap();
        assert_eq!(a.batteries.len(), 1);
        assert_eq!(a.batteries[0].component, Component::Right);
        assert!(parse(&[0x10, 0x05, 0x01]).is_none());
        assert!(parse(&[0x07, 0x19, 0x01]).is_none());
    }
}
