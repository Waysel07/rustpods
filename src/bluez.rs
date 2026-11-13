//! Finding AirPods through BlueZ and publishing their battery back to it.

use anyhow::{Result, bail};
use std::collections::HashMap;
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

/// Service UUID AirPods advertise for the AAP channel.
pub const AAP_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

#[derive(Debug, Clone)]
pub struct Device {
    pub path: OwnedObjectPath,
    pub adapter: OwnedObjectPath,
    pub address: String,
    pub name: String,
    pub product: u16,
    pub connected: bool,
}

fn prop<T: TryFrom<OwnedValue>>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    props.get(key).and_then(|v| v.try_clone().ok()).and_then(|v| T::try_from(v).ok())
}

/// "bluetooth:v004Cp2013dD415" -> (0x004c, 0x2013)
fn modalias(s: &str) -> Option<(u16, u16)> {
    let rest = s.strip_prefix("bluetooth:v")?;
    let vendor = u16::from_str_radix(rest.get(0..4)?, 16).ok()?;
    let product = u16::from_str_radix(rest.get(5..9)?, 16).ok()?;
    Some((vendor, product))
}

/// All paired Apple audio devices, connected ones first.
pub async fn airpods(system: &Connection) -> Result<Vec<Device>> {
    let om = ObjectManagerProxy::builder(system).destination("org.bluez")?.path("/")?.build().await?;
    let mut out = vec![];
    for (path, ifaces) in om.get_managed_objects().await? {
        let Some(props) = ifaces.iter().find(|(k, _)| k.as_str() == "org.bluez.Device1").map(|(_, v)| v) else {
            continue;
        };
        let uuids: Vec<String> = prop(props, "UUIDs").unwrap_or_default();
        let ids = prop::<String>(props, "Modalias").as_deref().and_then(modalias);
        let apple = ids.is_some_and(|(v, _)| v == 0x004c);
        if !apple && !uuids.iter().any(|u| u == AAP_UUID) {
            continue;
        }
        let adapter: OwnedObjectPath = prop(props, "Adapter").unwrap_or_else(|| {
            ObjectPath::try_from("/org/bluez/hci0").unwrap().into()
        });
        out.push(Device {
            path,
            adapter,
            address: prop(props, "Address").unwrap_or_default(),
            name: prop::<String>(props, "Alias").or_else(|| prop(props, "Name")).unwrap_or_else(|| "AirPods".into()),
            product: ids.map(|(_, p)| p).unwrap_or(0),
            connected: prop(props, "Connected").unwrap_or(false),
        });
    }
    out.sort_by_key(|d| !d.connected);
    Ok(out)
}

/// Picks a device by address or name fragment, or the first connected one.
pub async fn pick(system: &Connection, want: Option<&str>) -> Result<Device> {
    let all = airpods(system).await?;
    if let Some(w) = want {
        let wl = w.to_lowercase();
        if let Some(d) = all
            .iter()
            .find(|d| d.address.eq_ignore_ascii_case(w) || d.name.to_lowercase().contains(&wl))
        {
            return Ok(d.clone());
        }
        bail!("no paired AirPods match {w:?}");
    }
    match all.into_iter().find(|d| d.connected) {
        Some(d) => Ok(d),
        None => bail!("no AirPods connected (pair and connect them with bluetoothctl or your desktop first)"),
    }
}

// ---------------------------------------------------------------------------
// Battery provider: makes the level show up in BlueZ's Battery1, UPower and the
// desktop's Bluetooth/power menus.

pub const PROVIDER_ROOT: &str = "/dev/rustpods/battery";

pub struct BatteryProvider {
    pub device: OwnedObjectPath,
    pub percentage: u8,
}

#[zbus::interface(name = "org.bluez.BatteryProvider1")]
impl BatteryProvider {
    #[zbus(property)]
    fn device(&self) -> OwnedObjectPath {
        self.device.clone()
    }
    #[zbus(property)]
    fn percentage(&self) -> u8 {
        self.percentage
    }
    #[zbus(property)]
    fn source(&self) -> String {
        "rustpods (AirPods AAP)".into()
    }
}

pub fn provider_path(dev: &Device) -> String {
    format!("{PROVIDER_ROOT}/{}", dev.address.replace(':', "_"))
}

/// Registers our object tree with BlueZ. Objects can be added and removed later;
/// BlueZ follows them through the ObjectManager signals.
pub async fn register_provider(system: &Connection, adapter: &OwnedObjectPath) -> Result<()> {
    let srv = system.object_server();
    srv.at(PROVIDER_ROOT, zbus::fdo::ObjectManager).await?;
    system
        .call_method(
            Some("org.bluez"),
            adapter.as_str(),
            Some("org.bluez.BatteryProviderManager1"),
            "RegisterBatteryProvider",
            &(ObjectPath::try_from(PROVIDER_ROOT)?),
        )
        .await?;
    Ok(())
}

pub async fn set_battery(system: &Connection, dev: &Device, pct: Option<u8>) -> Result<()> {
    let srv = system.object_server();
    let path = provider_path(dev);
    match pct {
        None => {
            let _ = srv.remove::<BatteryProvider, _>(path.as_str()).await;
        }
        Some(p) => match srv.interface::<_, BatteryProvider>(path.as_str()).await {
            Ok(iface) => {
                let mut g = iface.get_mut().await;
                if g.percentage != p {
                    g.percentage = p;
                    g.percentage_changed(iface.signal_emitter()).await?;
                }
            }
            Err(_) => {
                srv.at(path.as_str(), BatteryProvider { device: dev.path.clone(), percentage: p }).await?;
            }
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn modalias_parses() {
        assert_eq!(super::modalias("bluetooth:v004Cp2013dD415"), Some((0x004c, 0x2013)));
        assert_eq!(super::modalias("usb:v054Cp0CE6d0100"), None);
    }
}
