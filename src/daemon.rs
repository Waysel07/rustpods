//! Background service: keeps the AAP channel open and reacts to what the AirPods report.

use crate::aap::{self, Battery, Charge, Component, Ear, Event, NoiseMode};
use crate::bluez::{self, Device};
use crate::l2cap::L2cap;
use crate::media;
use crate::popup::PopupData;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use zbus::Connection;
use zbus::zvariant::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub connected: bool,
    pub name: String,
    pub address: String,
    pub model: String,
    pub product: u16,
    pub batteries: Vec<Battery>,
    pub ear: Option<(Ear, Ear)>,
    pub noise: Option<NoiseMode>,
    pub noise_modes: Vec<NoiseMode>,
    pub conversation: Option<bool>,
}

impl State {
    fn for_device(d: &Device) -> State {
        State {
            connected: true,
            name: d.name.clone(),
            address: d.address.clone(),
            model: aap::model_name(d.product).into(),
            product: d.product,
            noise_modes: aap::noise_modes(d.product).to_vec(),
            ..Default::default()
        }
    }

    /// What to report to the desktop as "the" battery level: the emptier bud in use.
    pub fn headline(&self) -> Option<u8> {
        self.batteries
            .iter()
            .filter(|b| b.component != Component::Case && b.charge != Charge::Disconnected)
            .map(|b| b.level)
            .min()
    }

    pub fn popup(&self) -> PopupData {
        PopupData { name: self.name.clone(), model: self.model.clone(), product: self.product, batteries: self.batteries.clone() }
    }

    fn apply(&mut self, ev: &Event) {
        match ev {
            Event::Battery(b) if !b.is_empty() => {
                // Updates may list only some components; merge them.
                for nb in b {
                    match self.batteries.iter_mut().find(|x| x.component == nb.component) {
                        Some(x) => *x = *nb,
                        None => self.batteries.push(*nb),
                    }
                }
            }
            Event::Ear(a, b) => self.ear = Some((*a, *b)),
            Event::Noise(m) => self.noise = Some(*m),
            Event::Conversation(on) => self.conversation = Some(*on),
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub device: Option<String>,
    pub ear_pause: bool,
    pub popup: bool,
    pub notify_low: bool,
    pub battery_provider: bool,
    pub duck: bool,
    pub media_keys: bool,
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/rustpods-{}", unsafe { libc::getuid() })))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("rustpods.sock")
}

pub fn state_path() -> PathBuf {
    runtime_dir().join("rustpods.json")
}

// ---------------------------------------------------------------------------
// AAP session

pub async fn open(addr: &str) -> Result<L2cap> {
    let s = L2cap::connect(addr, aap::PSM).await?;
    s.send(&aap::HANDSHAKE).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    s.send(&aap::SET_FEATURES).await?;
    s.send(&aap::REQUEST_NOTIFICATIONS).await?;
    Ok(s)
}

/// One-shot query without the daemon: connect, collect a status snapshot, optionally send packets.
pub async fn direct(dev: &Device, send: &[Vec<u8>]) -> Result<State> {
    let s = open(&dev.address).await?;
    let mut st = State::for_device(dev);
    for p in send {
        s.send(p).await?;
    }
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_millis(5000);
    let nudge_at = started + Duration::from_millis(1500);
    let mut nudged = false;
    let mut got_battery_at: Option<tokio::time::Instant> = None;
    loop {
        let wait = match got_battery_at {
            Some(t) => (t + Duration::from_millis(400)).min(deadline),
            None if !nudged => nudge_at,
            None => deadline,
        };
        match tokio::time::timeout_at(wait, s.recv()).await {
            Ok(Ok(p)) if !p.is_empty() => {
                if let Some(ev) = aap::parse(&p) {
                    if matches!(ev, Event::Battery(_)) && got_battery_at.is_none() {
                        got_battery_at = Some(tokio::time::Instant::now());
                    }
                    st.apply(&ev);
                }
            }
            Ok(_) => bail!("AirPods closed the AAP channel"),
            // Some firmware is slow to send the first report: ask once more before giving up.
            Err(_) if got_battery_at.is_none() && !nudged => {
                nudged = true;
                s.send(&aap::REQUEST_NOTIFICATIONS).await?;
            }
            Err(_) => break,
        }
    }
    Ok(st)
}

// ---------------------------------------------------------------------------
// Desktop helpers

pub async fn notify(session: &Connection, summary: &str, body: &str) {
    let hints: HashMap<&str, Value> = HashMap::new();
    let _ = session
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("rustpods", 0u32, "audio-headphones", summary, body, Vec::<&str>::new(), hints, 5000i32),
        )
        .await;
}

fn spawn_popup(data: &PopupData) {
    let Ok(exe) = std::env::current_exe() else { return };
    let Ok(json) = serde_json::to_string(data) else { return };
    match std::process::Command::new(exe).args(["popup", "--data", &json]).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("popup: {e}"),
    }
}

fn write_state(st: &State) {
    if let Ok(j) = serde_json::to_vec(st) {
        let p = state_path();
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, j).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}

// ---------------------------------------------------------------------------
// The service

enum Cmd {
    Send(Vec<u8>, oneshot::Sender<Result<()>>),
}

pub async fn run(opts: Options) -> Result<()> {
    if ask("status").await.is_some() {
        bail!("another rustpods daemon is already running (systemctl --user status rustpods)");
    }
    let system = Connection::system().await?;
    let session = Connection::session().await?;
    let state = Arc::new(Mutex::new(State::default()));
    write_state(&State::default());

    let (tx, mut rx) = mpsc::channel::<Cmd>(8);
    tokio::spawn(ipc_server(state.clone(), tx));

    let mut provider_ok = false;
    let mut bridge_ok = false;
    let mut last_wait_msg = String::new();
    loop {
        let dev = match bluez::pick(&system, opts.device.as_deref()).await {
            Ok(d) if d.connected => d,
            Ok(d) => {
                wait_msg(&mut last_wait_msg, format!("waiting for {} to connect", d.name));
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            Err(e) => {
                wait_msg(&mut last_wait_msg, e.to_string());
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        if opts.media_keys && !bridge_ok {
            match crate::avrcp::register(&system, &session, &dev.adapter).await {
                Ok(()) => {
                    bridge_ok = true;
                    eprintln!("stem presses go to the active media player (double press = next)");
                }
                Err(e) => eprintln!("could not register media bridge ({e}); presses stay plain media keys"),
            }
        }
        if opts.battery_provider && !provider_ok {
            match bluez::register_provider(&system, &dev.adapter).await {
                Ok(()) => provider_ok = true,
                Err(e) => eprintln!("battery provider unavailable ({e}); desktop battery widgets won't see AirPods"),
            }
        }
        let sock = match open(&dev.address).await {
            Ok(s) => s,
            Err(e) => {
                // Right after the audio link comes up the AirPods sometimes refuse AAP for a moment.
                wait_msg(&mut last_wait_msg, format!("AAP not ready yet: {e:#}"));
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        last_wait_msg.clear();
        eprintln!("connected to {} ({}, {})", dev.name, aap::model_name(dev.product), dev.address);
        {
            let mut st = state.lock().await;
            *st = State::for_device(&dev);
            write_state(&st);
        }

        session_loop(&opts, &session, &system, &dev, &sock, &state, &mut rx, provider_ok).await;

        eprintln!("{} disconnected", dev.name);
        if provider_ok {
            let _ = bluez::set_battery(&system, &dev, None).await;
        }
        let mut st = state.lock().await;
        *st = State::default();
        write_state(&st);
        drop(st);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn wait_msg(last: &mut String, msg: String) {
    if *last != msg {
        eprintln!("{msg}");
        *last = msg;
    }
}

#[allow(clippy::too_many_arguments)]
async fn session_loop(
    opts: &Options,
    session: &Connection,
    system: &Connection,
    dev: &Device,
    sock: &L2cap,
    state: &Arc<Mutex<State>>,
    rx: &mut mpsc::Receiver<Cmd>,
    provider_ok: bool,
) {
    let mut popped = false;
    let mut warned: HashSet<(Component, u8)> = HashSet::new();
    let mut wearing_prev: Option<usize> = None;
    let mut paused: Vec<String> = vec![];
    let mut resume_level = 0usize;
    let mut ducked: Option<f32> = None;

    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        let pkt = tokio::select! {
            _ = tick.tick() => {
                if opts.media_keys {
                    crate::avrcp::sync_status(system, session).await;
                }
                continue;
            }
            r = sock.recv() => match r {
                Ok(p) if !p.is_empty() => p,
                _ => break,
            },
            Some(cmd) = rx.recv() => {
                match cmd {
                    Cmd::Send(p, reply) => { let _ = reply.send(sock.send(&p).await); }
                }
                continue;
            }
        };
        let Some(ev) = aap::parse(&pkt) else { continue };
        let snapshot = {
            let mut st = state.lock().await;
            st.apply(&ev);
            write_state(&st);
            st.clone()
        };

        match &ev {
            Event::Battery(_) => {
                if provider_ok {
                    if let Err(e) = bluez::set_battery(system, dev, snapshot.headline()).await {
                        eprintln!("battery provider: {e}");
                    }
                }
                if opts.popup && !popped && !snapshot.batteries.is_empty() {
                    popped = true;
                    spawn_popup(&snapshot.popup());
                }
                if opts.notify_low {
                    for b in &snapshot.batteries {
                        if b.component == Component::Case || b.charge != Charge::Discharging {
                            continue;
                        }
                        for limit in [20u8, 10] {
                            if b.level <= limit && warned.insert((b.component, limit)) {
                                let side = match b.component {
                                    Component::Left => "Left AirPod",
                                    Component::Right => "Right AirPod",
                                    _ => "AirPods",
                                };
                                notify(session, &format!("{} low battery", snapshot.name), &format!("{side}: {}%", b.level)).await;
                            }
                        }
                    }
                }
            }
            Event::Ear(a, b) => {
                let wearing = [a, b].iter().filter(|e| ***e == Ear::In).count();
                let in_case = [a, b].iter().all(|e| **e == Ear::Case);
                if let Some(prev) = wearing_prev {
                    if opts.ear_pause && wearing < prev && paused.is_empty() {
                        paused = media::pause_all(session).await.unwrap_or_default();
                        resume_level = prev;
                        if !paused.is_empty() {
                            eprintln!("ear detection: paused {}", paused.join(", "));
                        }
                    } else if wearing > prev && !paused.is_empty() && wearing >= resume_level {
                        media::resume(session, &paused).await;
                        eprintln!("ear detection: resumed");
                        paused.clear();
                    }
                    if in_case {
                        paused.clear();
                    }
                }
                wearing_prev = Some(wearing);
            }
            Event::Speaking(talking) if opts.duck => {
                if *talking && ducked.is_none() {
                    if let Some(v) = media::volume() {
                        ducked = Some(v);
                        media::set_volume(v * 0.3);
                    }
                } else if !*talking {
                    if let Some(v) = ducked.take() {
                        media::set_volume(v);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(v) = ducked {
        media::set_volume(v);
    }
}

// ---------------------------------------------------------------------------
// IPC: one line in, one JSON line out, over $XDG_RUNTIME_DIR/rustpods.sock

async fn ipc_server(state: Arc<Mutex<State>>, tx: mpsc::Sender<Cmd>) {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("control socket {}: {e}", path.display());
            return;
        }
    };
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (state, tx) = (state.clone(), tx.clone());
        tokio::spawn(async move {
            let _ = handle(stream, state, tx).await;
        });
    }
}

async fn handle(stream: UnixStream, state: Arc<Mutex<State>>, tx: mpsc::Sender<Cmd>) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let reply = match command(line.trim(), &state, &tx).await {
        Ok(v) => serde_json::json!({ "ok": true, "state": v }),
        Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
    };
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    Ok(())
}

async fn command(line: &str, state: &Arc<Mutex<State>>, tx: &mpsc::Sender<Cmd>) -> Result<State> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("status");
    let arg = parts.next().unwrap_or("");
    let st = state.lock().await.clone();
    let packet = match cmd {
        "status" => return Ok(st),
        "popup" => {
            if !st.connected {
                bail!("AirPods not connected");
            }
            spawn_popup(&st.popup());
            return Ok(st);
        }
        "noise" => {
            let m = NoiseMode::parse(arg).ok_or_else(|| anyhow!("unknown mode {arg:?}"))?;
            if !st.noise_modes.contains(&m) {
                bail!("{} does not support {m:?}", st.model);
            }
            aap::set_noise(m)
        }
        "conversation" => {
            if !aap::has_conversation_awareness(st.product) {
                bail!("{} has no conversational awareness", st.model);
            }
            aap::set_conversation(matches!(arg, "on" | "1" | "true"))
        }
        _ => bail!("unknown command {cmd:?}"),
    };
    if !st.connected {
        bail!("AirPods not connected");
    }
    let (rtx, rrx) = oneshot::channel();
    tx.send(Cmd::Send(packet, rtx)).await.map_err(|_| anyhow!("daemon busy"))?;
    rrx.await??;
    tokio::time::sleep(Duration::from_millis(250)).await;
    Ok(state.lock().await.clone())
}

/// Client side: talk to a running daemon. None when no daemon is listening.
pub async fn ask(line: &str) -> Option<Result<State>> {
    let mut s = UnixStream::connect(socket_path()).await.ok()?;
    s.write_all(format!("{line}\n").as_bytes()).await.ok()?;
    let mut out = String::new();
    BufReader::new(s).read_line(&mut out).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    if v["ok"].as_bool() == Some(true) {
        Some(serde_json::from_value(v["state"].clone()).map_err(Into::into))
    } else {
        Some(Err(anyhow!("{}", v["error"].as_str().unwrap_or("daemon error"))))
    }
}
