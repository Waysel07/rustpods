mod aap;
mod avrcp;
mod ble;
mod bluez;
mod config;
mod daemon;
mod l2cap;
mod media;
mod popup;
mod settings;
mod setup;

use aap::{Charge, Component, Ear, NoiseMode};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use daemon::State;
use std::process::Command;
use zbus::Connection;

/// Make AirPods feel at home on Linux: battery, ear detection, noise control, popup.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// AirPods to use: Bluetooth address or part of the name (default: first connected)
    #[arg(short, long, global = true)]
    device: Option<String>,
    /// Without a command, the settings window opens.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the background: battery for the desktop, auto-pause, popup, low-battery alerts
    Daemon {
        /// Don't pause media when an AirPod is taken out
        #[arg(long)]
        no_ear_pause: bool,
        /// Don't show the battery popup on connect
        #[arg(long)]
        no_popup: bool,
        /// Don't send low-battery notifications
        #[arg(long)]
        no_notify: bool,
        /// Don't publish battery to BlueZ/UPower
        #[arg(long)]
        no_battery_provider: bool,
        /// Lower the volume while you speak (conversational awareness, AirPods Pro 2 / 4 ANC)
        #[arg(long)]
        duck: bool,
        /// Leave stem presses (double = next, triple = previous) to the desktop's media keys
        #[arg(long)]
        no_media_keys: bool,
    },
    /// Battery, ear detection and noise mode
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Switch noise control: off, anc, transparency, adaptive (or "next" to cycle)
    Noise { mode: String },
    /// Turn conversational awareness on or off
    Conversation { state: String },
    /// Audio profile: "music" (A2DP, best codec) or "call" (headset with microphone); no argument lists them
    Profile { kind: Option<String> },
    /// Show the battery popup now
    Popup {
        /// Show sample data (no AirPods needed)
        #[arg(long)]
        demo: bool,
        /// Render to a PNG instead of showing it
        #[arg(long, value_name = "FILE")]
        png: Option<String>,
        #[arg(long, hide = true)]
        data: Option<String>,
    },
    /// Open the settings window (stem presses per side, noise control, automatic features)
    Settings,
    /// One line of JSON for a waybar custom module
    Waybar,
    /// Check the Bluetooth and audio setup and suggest fixes
    Doctor,
    /// Install and start the systemd user service
    Install,
    /// List paired AirPods / Beats
    List,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("rustpods: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let dev = cli.device.clone();
    let Some(cmd) = cli.cmd else {
        return settings::run();
    };
    match cmd {
        Cmd::Daemon { no_ear_pause, no_popup, no_notify, no_battery_provider, duck, no_media_keys } => {
            daemon::run(daemon::Options {
                device: dev,
                ear_pause: !no_ear_pause,
                popup: !no_popup,
                notify_low: !no_notify,
                battery_provider: !no_battery_provider,
                duck,
                media_keys: !no_media_keys,
            })
            .await
        }
        Cmd::Status { json } => {
            let st = status(dev.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                print_status(&st);
            }
            Ok(())
        }
        Cmd::Noise { mode } => {
            let mode = if mode == "next" {
                let st = status(dev.as_deref()).await?;
                let modes = &st.noise_modes;
                if modes.is_empty() {
                    bail!("{} has no noise control", st.model);
                }
                let i = st.noise.and_then(|n| modes.iter().position(|m| *m == n)).map_or(0, |i| (i + 1) % modes.len());
                modes[i]
            } else {
                NoiseMode::parse(&mode).ok_or_else(|| anyhow!("unknown mode {mode:?} (off, anc, transparency, adaptive, next)"))?
            };
            let st = control(dev.as_deref(), &format!("noise {}", mode_word(mode)), |d| {
                if !aap::noise_modes(d.product).contains(&mode) {
                    bail!("{} does not support {}", aap::model_name(d.product), mode_word(mode));
                }
                Ok(aap::set_noise(mode))
            })
            .await?;
            println!("Noise control: {}", st.noise.map(mode_word).unwrap_or(mode_word(mode)));
            Ok(())
        }
        Cmd::Conversation { state } => {
            let on = match state.as_str() {
                "on" | "1" | "true" => true,
                "off" | "0" | "false" => false,
                _ => bail!("use on or off"),
            };
            control(dev.as_deref(), &format!("conversation {}", if on { "on" } else { "off" }), |d| {
                if !aap::has_conversation_awareness(d.product) {
                    bail!("{} has no conversational awareness", aap::model_name(d.product));
                }
                Ok(aap::set_conversation(on))
            })
            .await?;
            println!("Conversational awareness: {}", if on { "on" } else { "off" });
            Ok(())
        }
        Cmd::Profile { kind } => profile(dev.as_deref(), kind.as_deref()).await,
        Cmd::Popup { demo, png, data } => {
            let data: popup::PopupData = if let Some(j) = data {
                serde_json::from_str(&j)?
            } else if demo {
                demo_data()
            } else {
                if png.is_none() && dev.is_none() {
                    if let Some(r) = daemon::ask("popup").await {
                        return r.map(|_| ());
                    }
                }
                status(dev.as_deref()).await?.popup()
            };
            if let Some(path) = png {
                popup::render_png(&data, &path, 2)?;
                println!("wrote {path}");
                return Ok(());
            }
            if let Err(e) = popup::show(data.clone()) {
                // GNOME, X11 or no compositor support: a notification is the next best thing.
                eprintln!("popup: {e:#}");
                let session = Connection::session().await?;
                daemon::notify(&session, &data.name, &battery_line(&data.batteries)).await;
            }
            Ok(())
        }
        Cmd::Settings => settings::run(),
        Cmd::Waybar => {
            let st = match daemon::ask("status").await {
                Some(r) => r.unwrap_or_default(),
                None => State::default(),
            };
            println!("{}", waybar(&st));
            Ok(())
        }
        Cmd::Doctor => doctor(dev.as_deref()).await,
        Cmd::Install => install(),
        Cmd::List => {
            let system = Connection::system().await?;
            let all = bluez::airpods(&system).await?;
            if all.is_empty() {
                println!("No AirPods or Beats paired.");
            }
            for d in all {
                println!(
                    "{}  {:<28} {:<24} {}",
                    d.address,
                    d.name,
                    aap::model_name(d.product),
                    if d.connected { "connected" } else { "" }
                );
            }
            Ok(())
        }
    }
}

fn mode_word(m: NoiseMode) -> &'static str {
    match m {
        NoiseMode::Off => "off",
        NoiseMode::Anc => "anc",
        NoiseMode::Transparency => "transparency",
        NoiseMode::Adaptive => "adaptive",
    }
}

/// Status from the daemon if it runs, otherwise straight from the AirPods.
async fn status(want: Option<&str>) -> Result<State> {
    if want.is_none() {
        if let Some(r) = daemon::ask("status").await {
            let st = r?;
            if !st.connected {
                bail!("AirPods not connected (daemon is running and waiting for them)");
            }
            return Ok(st);
        }
    }
    let system = Connection::system().await?;
    let d = bluez::pick(&system, want).await?;
    daemon::direct(&d, &[]).await
}

async fn control(want: Option<&str>, line: &str, packet: impl Fn(&bluez::Device) -> Result<Vec<u8>>) -> Result<State> {
    if want.is_none() {
        if let Some(r) = daemon::ask(line).await {
            return r;
        }
    }
    let system = Connection::system().await?;
    let d = bluez::pick(&system, want).await?;
    let p = packet(&d)?;
    daemon::direct(&d, &[p]).await
}

fn bar(level: u8) -> String {
    let n = (level as usize + 5) / 10;
    format!("{}{}", "█".repeat(n), "░".repeat(10 - n))
}

fn print_status(st: &State) {
    println!("{}  ({}, {})", st.name, st.model, st.address);
    if st.batteries.is_empty() {
        println!("  battery: no report yet");
    }
    let order = [Component::Left, Component::Right, Component::Case, Component::Single];
    for c in order {
        let Some(b) = st.batteries.iter().find(|b| b.component == c) else { continue };
        let name = match c {
            Component::Left => "Left",
            Component::Right => "Right",
            Component::Case => "Case",
            Component::Single => "Battery",
        };
        match b.charge {
            Charge::Disconnected => println!("  {name:<8} –"),
            ch => println!(
                "  {name:<8} {} {:>3}%{}",
                bar(b.level),
                b.level,
                if ch == Charge::Charging { "  charging" } else { "" }
            ),
        }
    }
    if let Some((a, b)) = st.ear {
        let w = |e: Ear| match e {
            Ear::In => "in ear",
            Ear::Out => "out",
            Ear::Case => "in case",
        };
        println!("  Ears     primary {}, secondary {}", w(a), w(b));
    }
    if !st.noise_modes.is_empty() {
        let modes: Vec<_> = st.noise_modes.iter().map(|m| mode_word(*m)).collect();
        println!("  Noise    {}  (available: {})", st.noise.map(mode_word).unwrap_or("?"), modes.join(", "));
    }
    if let Some(c) = st.conversation {
        println!("  Conversational awareness {}", if c { "on" } else { "off" });
    }
}

fn battery_line(b: &[aap::Battery]) -> String {
    let mut parts = vec![];
    for c in [Component::Left, Component::Right, Component::Case, Component::Single] {
        if let Some(x) = b.iter().find(|x| x.component == c && x.charge != Charge::Disconnected) {
            let n = match c {
                Component::Left => "L",
                Component::Right => "R",
                Component::Case => "Case",
                Component::Single => "",
            };
            parts.push(format!("{n} {}%{}", x.level, if x.charge == Charge::Charging { "⚡" } else { "" }));
        }
    }
    if parts.is_empty() { "Connected".into() } else { parts.join("  ") }
}

fn waybar(st: &State) -> serde_json::Value {
    if !st.connected {
        return serde_json::json!({ "text": "", "class": "disconnected", "tooltip": "AirPods not connected" });
    }
    let pct = st.headline();
    let class = match pct {
        Some(p) if p <= 10 => "critical",
        Some(p) if p <= 20 => "low",
        _ => "connected",
    };
    let mut tip = format!("{} ({})\n{}", st.name, st.model, battery_line(&st.batteries));
    if let Some(n) = st.noise {
        tip.push_str(&format!("\nNoise: {}", mode_word(n)));
    }
    serde_json::json!({
        "text": pct.map(|p| format!("{p}%")).unwrap_or_default(),
        "percentage": pct.unwrap_or(0),
        "tooltip": tip,
        "class": class,
        "alt": st.noise.map(mode_word).unwrap_or("none"),
    })
}

fn demo_data() -> popup::PopupData {
    use aap::Battery;
    popup::PopupData {
        name: "AirPods".into(),
        model: "AirPods Pro 2".into(),
        product: 0x2014,
        connect: None,
        batteries: vec![
            Battery { component: Component::Left, level: 82, charge: Charge::Discharging },
            Battery { component: Component::Right, level: 15, charge: Charge::Discharging },
            Battery { component: Component::Case, level: 64, charge: Charge::Charging },
        ],
    }
}

async fn profile(want: Option<&str>, kind: Option<&str>) -> Result<()> {
    let system = Connection::system().await?;
    let d = bluez::pick(&system, want).await?;
    let (list, active) =
        media::profiles(&d.address).context("no PipeWire/PulseAudio card for these AirPods (is pipewire-pulse running?)")?;
    let Some(kind) = kind else {
        for p in &list {
            println!("{} {:<24} {}", if p.name == active { "*" } else { " " }, p.name, p.description);
        }
        return Ok(());
    };
    let prefix = match kind {
        "music" | "hifi" | "a2dp" => "a2dp",
        "call" | "mic" | "headset" | "hfp" => "headset",
        "off" => "off",
        _ => bail!("use music, call or off"),
    };
    let p = media::best_profile(&list, prefix).ok_or_else(|| anyhow!("no {kind} profile available"))?;
    media::set_profile(&d.address, &p.name)?;
    println!("{} → {}", d.name, p.description);
    Ok(())
}

// ---------------------------------------------------------------------------

fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

async fn doctor(want: Option<&str>) -> Result<()> {
    let ok = |s: &str| println!("  \x1b[32m✓\x1b[0m {s}");
    let warn = |s: &str| println!("  \x1b[33m!\x1b[0m {s}");
    let bad = |s: &str| println!("  \x1b[31m✗\x1b[0m {s}");
    let hint = |s: &str| println!("      {s}");

    println!("Bluetooth");
    if sh("systemctl", &["is-active", "bluetooth"]).as_deref() == Some("active") {
        ok("bluetooth.service is running");
    } else {
        bad("bluetooth.service is not running");
        hint("sudo systemctl enable --now bluetooth");
    }
    let system = Connection::system().await?;
    let conf = std::fs::read_to_string("/etc/bluetooth/main.conf").unwrap_or_default();
    let setting = |key: &str| {
        conf.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()))
    };
    let all = bluez::airpods(&system).await.unwrap_or_default();
    let dev = match want {
        Some(w) => bluez::pick(&system, Some(w)).await.ok(),
        None => all.iter().find(|d| d.connected).or(all.first()).cloned(),
    };
    match &dev {
        None => {
            bad("no AirPods paired");
            hint("Put them in the open case, hold the button until the light flashes white, then:");
            hint("bluetoothctl scan on; bluetoothctl pair <address>; bluetoothctl trust <address>; bluetoothctl connect <address>");
        }
        Some(d) => {
            ok(&format!("paired: {} ({}, {})", d.name, aap::model_name(d.product), d.address));
            if d.connected {
                ok("connected");
            } else {
                warn("not connected right now");
                hint(&format!("bluetoothctl connect {}", d.address));
            }
        }
    }
    let adapter = dev.as_ref().map(|d| d.adapter.to_string()).unwrap_or("/org/bluez/hci0".into());
    let introspect = sh("busctl", &["introspect", "org.bluez", &adapter]).unwrap_or_default();
    if introspect.contains("BatteryProviderManager1") {
        ok("BlueZ accepts battery providers (AirPods battery can show in desktop widgets)");
    } else {
        warn("BlueZ has no BatteryProviderManager1, so the desktop won't see AirPods battery");
        hint("Add `Experimental = true` under [General] in /etc/bluetooth/main.conf, then restart bluetooth");
    }
    match setting("ControllerMode").as_deref() {
        Some("bredr") => ok("ControllerMode = bredr (helps older AirPods that fail to pair or reconnect)"),
        _ => {
            warn("ControllerMode is dual (default). Fine for most; if pairing or reconnecting fails, try");
            hint("ControllerMode = bredr   under [General] in /etc/bluetooth/main.conf (disables Bluetooth LE devices)");
        }
    }
    if setting("FastConnectable").as_deref() == Some("true") {
        ok("FastConnectable = true");
    } else {
        warn("FastConnectable is off: reconnecting after taking the AirPods out of the case is slower");
        hint("FastConnectable = true   under [General] in /etc/bluetooth/main.conf (uses a bit more power)");
    }
    if let Some(id) = setting("DeviceID") {
        ok(&format!("DeviceID = {id}"));
    }

    println!("\nAudio");
    if let Some(d) = dev.as_ref().filter(|d| d.connected) {
        match media::profiles(&d.address) {
            None => {
                bad("no audio card for the AirPods");
                hint("Is PipeWire running?  systemctl --user status pipewire wireplumber pipewire-pulse");
            }
            Some((list, active)) => {
                let desc = list.iter().find(|p| p.name == active).map(|p| p.description.clone()).unwrap_or(active.clone());
                if active.starts_with("a2dp") {
                    ok(&format!("profile: {desc}"));
                } else if active.starts_with("headset") {
                    warn(&format!("profile: {desc} (mono, phone-call quality)"));
                    hint("rustpods profile music");
                } else {
                    bad(&format!("profile: {desc}"));
                    hint("rustpods profile music");
                }
                if list.iter().any(|p| p.description.contains("AAC")) {
                    ok("AAC codec available");
                } else {
                    warn("AAC codec not offered; AirPods sound best with AAC");
                    hint("Install PipeWire's AAC support (libfdk-aac) and reconnect");
                }
                if !list.iter().any(|p| p.name.contains("msbc") || p.description.contains("MSBC")) {
                    warn("mSBC not available: calls will use narrowband CVSD");
                }
            }
        }
    } else {
        warn("skipped (AirPods not connected)");
    }

    println!("\nApple features (AAP)");
    match (daemon::ask("status").await, &dev) {
        (Some(Ok(st)), _) if st.connected => ok("rustpods daemon is running and talking to the AirPods"),
        (Some(_), _) => warn("rustpods daemon is running but not connected to AirPods yet"),
        (None, Some(d)) if d.connected => {
            match daemon::direct(d, &[]).await {
                Ok(st) if !st.batteries.is_empty() => ok(&format!("AAP works: {}", battery_line(&st.batteries))),
                Ok(_) => warn("AAP channel opened but no battery report arrived"),
                Err(e) => {
                    bad(&format!("could not open AAP channel: {e:#}"));
                    hint("Reconnect the AirPods. Some firmware only answers devices with an Apple DeviceID:");
                    hint("DeviceID = bluetooth:004C:0000:0000   under [General] in /etc/bluetooth/main.conf");
                }
            }
            warn("rustpods daemon not running: no auto-pause, popup or desktop battery");
            hint("rustpods install");
        }
        _ => warn("rustpods daemon not running"),
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_uppercase();
    if desktop.contains("GNOME") {
        warn("GNOME has no layer-shell: the popup falls back to a notification");
    } else if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        warn("not a Wayland session: the popup falls back to a notification");
    } else {
        ok("Wayland session: battery popup supported");
    }
    Ok(())
}

fn install() -> Result<()> {
    let exe = std::env::current_exe()?;
    let home = std::env::var("HOME")?;
    let dir = std::path::PathBuf::from(&home).join(".config/systemd/user");
    std::fs::create_dir_all(&dir)?;
    let unit = format!(
        "[Unit]\nDescription=rustpods: AirPods battery, ear detection and popup\n\
         After=graphical-session.target\nPartOf=graphical-session.target\n\n\
         [Service]\nExecStart={} daemon\nRestart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=graphical-session.target\n",
        exe.display()
    );
    let path = dir.join("rustpods.service");
    std::fs::write(&path, unit)?;
    println!("wrote {}", path.display());
    for args in [&["--user", "daemon-reload"][..], &["--user", "enable", "--now", "rustpods.service"]] {
        let st = Command::new("systemctl").args(args).status()?;
        if !st.success() {
            bail!("systemctl {} failed", args.join(" "));
        }
    }
    let apps = std::path::PathBuf::from(&home).join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=RustPods\nComment=AirPods settings: stem presses, noise control, battery\n\
         Exec={} settings\nIcon=audio-headphones\nTerminal=false\nCategories=Settings;HardwareSettings;AudioVideo;\n\
         Keywords=airpods;bluetooth;headphones;battery;\nStartupWMClass=rustpods\n",
        exe.display()
    );
    std::fs::write(apps.join("rustpods.desktop"), desktop)?;
    println!("added RustPods to your app launcher (or run: rustpods settings)");
    println!("rustpods is running. Logs: journalctl --user -u rustpods -f");
    Ok(())
}
