//! iPhone-style battery card that slides up from the bottom of the screen when the
//! AirPods connect. Pure Rust: a wlr-layer-shell surface (KDE Plasma, Sway,
//! Hyprland, niri, COSMIC, …) drawn in software with tiny-skia and ab_glyph.

use crate::aap::{Battery, Charge, Component};
use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::client::{
        Connection, QueueHandle,
        globals::registry_queue_init,
        protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PopupData {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub product: u16,
    pub batteries: Vec<Battery>,
    /// Connection assistant: the paired address to connect when the button is pressed.
    #[serde(default)]
    pub connect: Option<String>,
}

const W: u32 = 400;
const H: u32 = 310;
const SHOW: Duration = Duration::from_millis(6500);
/// The assistant waits longer: you need time to reach for the button.
const SHOW_ASSIST: Duration = Duration::from_secs(15);
/// Extra height for the Connect button.
const BUTTON_AREA: u32 = 76;
const BUTTON_Y: f32 = 318.0;
const BUTTON_H: f32 = 48.0;
const BUTTON_X: f32 = 24.0;
const ACCENT: (u8, u8, u8) = (0, 122, 255);

fn height(d: &PopupData) -> u32 {
    if d.connect.is_some() { H + BUTTON_AREA } else { H }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Conn {
    Idle,
    Connecting,
    Done,
    Failed,
}

impl Conn {
    fn label(self) -> &'static str {
        match self {
            Conn::Idle => "Connect",
            Conn::Connecting => "Connecting…",
            Conn::Done => "Connected",
            Conn::Failed => "Couldn't connect. Try again",
        }
    }
}

/// Asks BlueZ to connect the paired AirPods (bluetoothctl ships with BlueZ everywhere).
fn connect(addr: String, state: Arc<Mutex<Conn>>) {
    std::thread::spawn(move || {
        let ok = std::process::Command::new("timeout")
            .args(["20", "bluetoothctl", "connect", &addr])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Connection successful"));
        *state.lock().unwrap() = if ok { Conn::Done } else { Conn::Failed };
    });
}
const IN: f32 = 0.32;
const OUT: f32 = 0.26;

// iOS light sheet palette.
const SHEET: (u8, u8, u8) = (255, 255, 255);
const INK: (u8, u8, u8) = (0, 0, 0);
const TEXT: (u8, u8, u8) = (28, 28, 30);
const GRAY: (u8, u8, u8) = (110, 110, 115);
const CLOSE_BG: (u8, u8, u8) = (233, 233, 235);
const LINE: (u8, u8, u8) = (199, 199, 204);
const GREEN: (u8, u8, u8) = (52, 199, 89);
const RED: (u8, u8, u8) = (255, 59, 48);
const AMBER: (u8, u8, u8) = (255, 159, 10);

pub fn show(data: PopupData) -> Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(anyhow!("no Wayland session"));
    }
    let fonts = Fonts::load()?;
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("compositor has no wlr-layer-shell (GNOME does not support it); falling back to a notification")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("rustpods"), None);
    layer.set_anchor(Anchor::BOTTOM);
    layer.set_margin(0, 0, 64, 0);
    layer.set_size(W, height(&data));
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_exclusive_zone(-1);
    layer.commit();

    let mut app = Popup {
        registry: RegistryState::new(&globals),
        seat: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),
        pool: SlotPool::new((W * height(&data) * 4 * 4) as usize, &shm)?,
        shm,
        layer,
        pointer: None,
        scale: 1,
        card: None,
        data,
        fonts,
        configured: false,
        start: Instant::now(),
        hide_at: None,
        conn: Arc::new(Mutex::new(Conn::Idle)),
        drawn_conn: Conn::Idle,
        pointer_at: (0.0, 0.0),
        exit: false,
    };
    while !app.exit {
        queue.blocking_dispatch(&mut app)?;
    }
    Ok(())
}

struct Popup {
    registry: RegistryState,
    seat: SeatState,
    output: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    pointer: Option<wl_pointer::WlPointer>,
    scale: i32,
    card: Option<Pixmap>,
    data: PopupData,
    fonts: Fonts,
    configured: bool,
    start: Instant,
    hide_at: Option<Instant>,
    conn: Arc<Mutex<Conn>>,
    drawn_conn: Conn,
    pointer_at: (f64, f64),
    exit: bool,
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

impl Popup {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let s = self.scale.max(1) as u32;
        let (w, h) = (W * s, height(&self.data) * s);
        let conn = *self.conn.lock().unwrap();
        let now = Instant::now();
        if conn != self.drawn_conn {
            self.drawn_conn = conn;
            self.card = None;
            match conn {
                // Stay up while connecting; leave shortly after it worked.
                Conn::Connecting => self.hide_at = Some(now + Duration::from_secs(25)),
                Conn::Done => self.hide_at = Some(now + Duration::from_millis(1200)),
                Conn::Failed => self.hide_at = Some(now + Duration::from_secs(8)),
                Conn::Idle => {}
            }
        }
        let button = self.data.connect.as_ref().map(|_| conn);
        if self.card.as_ref().is_none_or(|c| c.width() != w) {
            self.card = render_card(&self.data, &self.fonts, s as f32, button);
        }
        let Some(card) = &self.card else {
            self.exit = true;
            return;
        };

        // Animation: slide up + fade in, hold, fade + sink out.
        let show = if self.data.connect.is_some() { SHOW_ASSIST } else { SHOW };
        let hide_at = *self.hide_at.get_or_insert(self.start + show);
        let t_in = ease_out(now.duration_since(self.start).as_secs_f32() / IN);
        let t_out = if now >= hide_at { ease_out(now.duration_since(hide_at).as_secs_f32() / OUT) } else { 0.0 };
        if t_out >= 1.0 {
            self.exit = true;
            return;
        }
        let alpha = t_in * (1.0 - t_out);
        let dy = (1.0 - t_in) * 28.0 + t_out * 16.0;
        let zoom = 0.94 + 0.06 * t_in;

        let mut frame = Pixmap::new(w, h).unwrap();
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let tf = Transform::from_translate(-cx, -cy)
            .post_scale(zoom, zoom)
            .post_translate(cx, cy + dy * s as f32);
        frame.draw_pixmap(0, 0, card.as_ref(), &PixmapPaint { opacity: alpha, ..Default::default() }, tf, None);

        let Ok((buffer, canvas)) = self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888)
        else {
            self.exit = true;
            return;
        };
        // tiny-skia: premultiplied RGBA; wl_shm ARGB8888 little endian: B G R A.
        for (dst, src) in canvas.chunks_exact_mut(4).zip(frame.data().chunks_exact(4)) {
            dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
        }
        let surf = self.layer.wl_surface();
        surf.set_buffer_scale(s as i32);
        surf.damage_buffer(0, 0, w as i32, h as i32);
        surf.frame(qh, FrameCallbackData(surf.clone()));
        let _ = buffer.attach_to(surf);
        self.layer.commit();
    }
}

// ---------------------------------------------------------------------------
// Drawing

struct Fonts {
    bold: FontVec,
    regular: FontVec,
}

impl Fonts {
    fn load() -> Result<Fonts> {
        fn find(pattern: &str, fallbacks: &[&str]) -> Result<FontVec> {
            let mut paths: Vec<String> = vec![];
            if let Ok(o) = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output() {
                paths.push(String::from_utf8_lossy(&o.stdout).into_owned());
            }
            paths.extend(fallbacks.iter().map(|s| s.to_string()));
            for p in paths {
                if let Ok(bytes) = std::fs::read(&p) {
                    if let Ok(f) = FontVec::try_from_vec(bytes) {
                        return Ok(f);
                    }
                }
            }
            Err(anyhow!("no usable font for {pattern}"))
        }
        Ok(Fonts {
            bold: find(
                "sans-serif:bold",
                &["/usr/share/fonts/noto/NotoSans-Bold.ttf", "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf"],
            )?,
            regular: find(
                "sans-serif",
                &["/usr/share/fonts/noto/NotoSans-Regular.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf"],
            )?,
        })
    }
}

fn paint(c: (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(c.0, c.1, c.2, a));
    p.anti_alias = true;
    p
}

/// Glossy white plastic: soft top-to-bottom gradient like the product renders.
fn plastic(y0: f32, y1: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    if let Some(sh) = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, y0),
        tiny_skia::Point::from_xy(0.0, y1),
        vec![
            tiny_skia::GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 255)),
            tiny_skia::GradientStop::new(0.55, Color::from_rgba8(246, 246, 248, 255)),
            tiny_skia::GradientStop::new(1.0, Color::from_rgba8(218, 218, 223, 255)),
        ],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    ) {
        p.shader = sh;
    }
    p
}

fn rounded(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.4477 * r; // circle-ish corners with cubics
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - k, y, x + w, y + k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - k, x + w - k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + k, y + h, x, y + h - k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + k, x + k, y, x + r, y);
    pb.close();
}

fn fill(pm: &mut Pixmap, pb: PathBuilder, p: &Paint) {
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, p, FillRule::Winding, Transform::identity(), None);
    }
}

fn outline(pm: &mut Pixmap, pb: PathBuilder, c: (u8, u8, u8), a: u8, w: f32) {
    if let Some(path) = pb.finish() {
        pm.stroke_path(&path, &paint(c, a), &Stroke { width: w, ..Default::default() }, Transform::identity(), None);
    }
}

fn fill_outlined(pm: &mut Pixmap, pb: PathBuilder, p: &Paint, line_alpha: u8, u: f32) {
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, p, FillRule::Winding, Transform::identity(), None);
        pm.stroke_path(&path, &paint(LINE, line_alpha), &Stroke { width: u, ..Default::default() }, Transform::identity(), None);
    }
}

fn text_width(font: &FontVec, s: &str, px: f32) -> f32 {
    let sf = font.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for c in s.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            w += sf.kern(p, id);
        }
        w += sf.h_advance(id);
        prev = Some(id);
    }
    w
}

/// Draws `s` horizontally centred on `cx` with its baseline at `y`.
fn text(pm: &mut Pixmap, font: &FontVec, s: &str, px: f32, cx: f32, y: f32, c: (u8, u8, u8)) {
    let sf = font.as_scaled(PxScale::from(px));
    let mut x = cx - text_width(font, s, px) / 2.0;
    let mut prev = None;
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    for ch in s.chars() {
        let id = sf.glyph_id(ch);
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        let g = id.with_scale_and_position(px, point(x, y));
        x += sf.h_advance(id);
        prev = Some(id);
        let Some(outline) = font.outline_glyph(g) else { continue };
        let b = outline.px_bounds();
        let pixels = pm.data_mut();
        outline.draw(|gx, gy, cov| {
            let (px_, py_) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
            if px_ < 0 || py_ < 0 || px_ >= pw || py_ >= ph {
                return;
            }
            let i = ((py_ * pw + px_) * 4) as usize;
            let a = cov.clamp(0.0, 1.0);
            let src = [c.0 as f32 * a, c.1 as f32 * a, c.2 as f32 * a, 255.0 * a];
            for k in 0..4 {
                pixels[i + k] = (src[k] + pixels[i + k] as f32 * (1.0 - a)).round() as u8;
            }
        });
    }
}

/// One earbud. `m` = 1 for the right bud, -1 for the left (mirrored).
fn bud(pm: &mut Pixmap, x: f32, y: f32, u: f32, m: f32, tips: bool) {
    let (top, bottom) = (y - 44.0 * u, y + 56.0 * u);
    // Stem
    let mut stem = PathBuilder::new();
    rounded(&mut stem, x + (m * 6.0 - 8.0) * u, y - 18.0 * u, 16.0 * u, 74.0 * u, 8.0 * u);
    // Head
    let mut head = PathBuilder::new();
    head.push_circle(x, y - 22.0 * u, 23.0 * u);
    for pb in [stem, head] {
        fill_outlined(pm, pb, &plastic(top, bottom), 200, u);
    }
    // Re-fill the head over the stem's outline so they read as one piece.
    let mut head2 = PathBuilder::new();
    head2.push_circle(x, y - 22.0 * u, 22.4 * u);
    fill(pm, head2, &plastic(top, bottom));
    let mut neck = PathBuilder::new();
    rounded(&mut neck, x + (m * 6.0 - 7.4) * u, y - 6.0 * u, 14.8 * u, 12.0 * u, 2.0 * u);
    fill(pm, neck, &plastic(top, bottom));

    if tips {
        // Silicone ear tip, angled towards the ear.
        let mut tip = PathBuilder::new();
        tip.push_circle(x - m * 15.0 * u, y - 10.0 * u, 14.0 * u);
        fill(pm, tip, &plastic(top - 10.0 * u, bottom - 30.0 * u));
        let mut tip_o = PathBuilder::new();
        tip_o.push_circle(x - m * 15.0 * u, y - 10.0 * u, 14.0 * u);
        outline(pm, tip_o, LINE, 220, 1.0 * u);
        let mut hole = PathBuilder::new();
        hole.push_circle(x - m * 17.0 * u, y - 11.0 * u, 6.0 * u);
        fill(pm, hole, &paint((214, 214, 219), 255));
    } else {
        // Speaker mesh on the inner face.
        let mut mesh = PathBuilder::new();
        mesh.push_circle(x - m * 9.0 * u, y - 18.0 * u, 7.0 * u);
        fill(pm, mesh, &paint((150, 150, 156), 255));
    }
    // Black sensor/vent slot near the top.
    let mut vent = PathBuilder::new();
    rounded(&mut vent, x + (m * 5.0 - 5.5) * u, y - 39.0 * u, 11.0 * u, 5.0 * u, 2.5 * u);
    fill(pm, vent, &paint((30, 30, 32), 255));
    // Metal tip at the bottom of the stem.
    let mut cap = PathBuilder::new();
    rounded(&mut cap, x + (m * 6.0 - 6.0) * u, y + 50.0 * u, 12.0 * u, 5.0 * u, 2.5 * u);
    fill(pm, cap, &paint((176, 176, 182), 255));
}

fn case(pm: &mut Pixmap, cx: f32, cy: f32, u: f32, charging: bool) {
    let (w, h) = (134.0 * u, 100.0 * u);
    let (x, y) = (cx - w / 2.0, cy - h / 2.0);
    let mut shadow = PathBuilder::new();
    rounded(&mut shadow, x + 3.0 * u, y + 4.0 * u, w - 6.0 * u, h, 34.0 * u);
    fill(pm, shadow, &paint((0, 0, 0), 10));
    let mut body = PathBuilder::new();
    rounded(&mut body, x, y, w, h, 34.0 * u);
    fill_outlined(pm, body, &plastic(y, y + h), 200, u);
    // Lid seam
    let mut seam = PathBuilder::new();
    seam.move_to(x + 4.0 * u, y + 30.0 * u);
    seam.line_to(x + w - 4.0 * u, y + 30.0 * u);
    outline(pm, seam, (205, 205, 210), 255, 1.2 * u);
    // Status light
    let mut led = PathBuilder::new();
    led.push_circle(cx, cy + 10.0 * u, 2.2 * u);
    fill(pm, led, &paint(if charging { AMBER } else { GREEN }, 255));
}

fn headphones(pm: &mut Pixmap, cx: f32, cy: f32, u: f32) {
    let mut band = PathBuilder::new();
    band.move_to(cx - 44.0 * u, cy + 8.0 * u);
    band.cubic_to(cx - 44.0 * u, cy - 62.0 * u, cx + 44.0 * u, cy - 62.0 * u, cx + 44.0 * u, cy + 8.0 * u);
    if let Some(p) = band.finish() {
        pm.stroke_path(&p, &paint((214, 214, 219), 255), &Stroke { width: 9.0 * u, ..Default::default() }, Transform::identity(), None);
    }
    for dx in [-1.0f32, 1.0] {
        let mut cup = PathBuilder::new();
        rounded(&mut cup, cx + dx * 44.0 * u - 19.0 * u, cy - 10.0 * u, 38.0 * u, 62.0 * u, 16.0 * u);
        fill_outlined(pm, cup, &plastic(cy - 10.0 * u, cy + 52.0 * u), 220, u);
    }
}

/// Horizontal iOS battery glyph centred on `cx`, plus the percentage below it.
fn battery(pm: &mut Pixmap, fonts: &Fonts, b: &Battery, cx: f32, y: f32, u: f32) {
    let (w, h) = (34.0 * u, 16.0 * u);
    let (x, top) = (cx - w / 2.0 - 1.5 * u, y);
    let gone = b.charge == Charge::Disconnected;
    let mut shell = PathBuilder::new();
    rounded(&mut shell, x, top, w, h, 4.5 * u);
    outline(pm, shell, (174, 174, 178), 255, 1.6 * u);
    let mut nub = PathBuilder::new();
    rounded(&mut nub, x + w + 1.5 * u, top + 5.0 * u, 2.5 * u, 6.0 * u, 1.2 * u);
    fill(pm, nub, &paint((174, 174, 178), 255));
    if !gone {
        let lvl = b.level as f32 / 100.0;
        let inner = (w - 5.0 * u) * lvl.max(0.06);
        let mut f = PathBuilder::new();
        rounded(&mut f, x + 2.5 * u, top + 2.5 * u, inner, h - 5.0 * u, 2.5 * u);
        let col = if b.level <= 20 && b.charge != Charge::Charging { RED } else { GREEN };
        fill(pm, f, &paint(col, 255));
    }
    if b.charge == Charge::Charging {
        // Bolt with a white halo so it reads on top of the green fill.
        let (bx, by) = (cx - 5.0 * u, top - 3.0 * u);
        let mut bolt = PathBuilder::new();
        bolt.move_to(bx + 7.5 * u, by);
        bolt.line_to(bx, by + 12.5 * u);
        bolt.line_to(bx + 5.0 * u, by + 12.5 * u);
        bolt.line_to(bx + 3.0 * u, by + 22.0 * u);
        bolt.line_to(bx + 11.0 * u, by + 8.5 * u);
        bolt.line_to(bx + 6.0 * u, by + 8.5 * u);
        bolt.close();
        if let Some(p) = bolt.finish() {
            pm.stroke_path(&p, &paint(SHEET, 255), &Stroke { width: 2.4 * u, line_join: tiny_skia::LineJoin::Round, ..Default::default() }, Transform::identity(), None);
            pm.fill_path(&p, &paint(INK, 255), FillRule::Winding, Transform::identity(), None);
        }
    }
    let pct = if gone { "–".to_string() } else { format!("{}%", b.level) };
    text(pm, &fonts.regular, &pct, 19.0 * u, cx, y + 44.0 * u, TEXT);
}

enum Group {
    Pair(Battery),
    Bud(Battery),
    Case(Battery),
    Single(Battery),
}

/// Like iOS: one entry for both buds when they agree, separate ones when they don't.
fn groups(bs: &[Battery]) -> Vec<Group> {
    let get = |c| bs.iter().find(|b| b.component == c).copied();
    let mut out = vec![];
    if let Some(s) = get(Component::Single) {
        out.push(Group::Single(s));
    }
    match (get(Component::Left), get(Component::Right)) {
        (Some(l), Some(r))
            if l.charge == r.charge && (l.level as i16 - r.level as i16).abs() <= 10 =>
        {
            out.push(Group::Pair(if l.level <= r.level { l } else { r }));
        }
        (l, r) => {
            for b in [l, r].into_iter().flatten() {
                if b.charge != Charge::Disconnected {
                    out.push(Group::Bud(b));
                }
            }
        }
    }
    if let Some(c) = get(Component::Case).filter(|c| c.charge != Charge::Disconnected) {
        out.push(Group::Case(c));
    }
    out
}

fn has_tips(product: u16) -> bool {
    matches!(product, 0x200e | 0x2014 | 0x2024 | 0x2027)
}

/// Directory for user supplied product pictures:
/// `~/.config/rustpods/images/<product id>/{buds,left,right,case,headphones}.png`, falling back to
/// the same names directly in `~/.config/rustpods/images/`. The product id is the 4 hex digits
/// from `rustpods status --json` / BlueZ's Modalias, e.g. `2014` for AirPods Pro 2.
pub fn image_dirs(product: u16) -> Vec<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("rustpods/images");
    vec![base.join(format!("{product:04x}")), base]
}

/// Draws a user image fitted into a `bw`×`bh` box centred on (cx, cy). False if none exists.
fn user_image(pm: &mut Pixmap, product: u16, names: &[&str], cx: f32, cy: f32, bw: f32, bh: f32) -> bool {
    for dir in image_dirs(product) {
        for n in names {
            let Ok(img) = Pixmap::load_png(dir.join(format!("{n}.png"))) else { continue };
            let k = (bw / img.width() as f32).min(bh / img.height() as f32);
            let (w, h) = (img.width() as f32 * k, img.height() as f32 * k);
            let tf = Transform::from_scale(k, k).post_translate(cx - w / 2.0, cy - h / 2.0);
            let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
            pm.draw_pixmap(0, 0, img.as_ref(), &paint, tf, None);
            return true;
        }
    }
    false
}

fn render_card(data: &PopupData, fonts: &Fonts, u: f32, button: Option<Conn>) -> Option<Pixmap> {
    let (w, h) = (W as f32 * u, height(data) as f32 * u);
    let mut pm = Pixmap::new(w as u32, h as u32)?;
    let mut sheet = PathBuilder::new();
    rounded(&mut sheet, 0.0, 0.0, w, h, 42.0 * u);
    fill(&mut pm, sheet, &paint(SHEET, 255));
    let mut edge = PathBuilder::new();
    rounded(&mut edge, 0.5 * u, 0.5 * u, w - u, h - u, 41.5 * u);
    outline(&mut pm, edge, INK, 30, u);

    // Close button
    let (bx, by) = (w - 38.0 * u, 38.0 * u);
    let mut circle = PathBuilder::new();
    circle.push_circle(bx, by, 15.0 * u);
    fill(&mut pm, circle, &paint(CLOSE_BG, 255));
    let mut x = PathBuilder::new();
    let d = 5.0 * u;
    x.move_to(bx - d, by - d);
    x.line_to(bx + d, by + d);
    x.move_to(bx + d, by - d);
    x.line_to(bx - d, by + d);
    if let Some(p) = x.finish() {
        let st = Stroke { width: 2.2 * u, line_cap: tiny_skia::LineCap::Round, ..Default::default() };
        pm.stroke_path(&p, &paint(GRAY, 255), &st, Transform::identity(), None);
    }

    // Title
    let mut name = data.name.clone();
    let px = 25.0 * u;
    while text_width(&fonts.bold, &name, px) > w - 110.0 * u && name.chars().count() > 4 {
        name.pop();
        name = format!("{}…", name.trim_end_matches('…'));
    }
    text(&mut pm, &fonts.bold, &name, px, w / 2.0, 82.0 * u, INK);

    let gs = groups(&data.batteries);
    if gs.is_empty() {
        if !user_image(&mut pm, data.product, &["buds"], w / 2.0, 160.0 * u, 160.0 * u, 130.0 * u) {
            bud(&mut pm, w / 2.0 - 30.0 * u, 165.0 * u, u, -1.0, has_tips(data.product));
            bud(&mut pm, w / 2.0 + 30.0 * u, 165.0 * u, u, 1.0, has_tips(data.product));
        }
        text(&mut pm, &fonts.regular, "Connected", 19.0 * u, w / 2.0, 272.0 * u, GRAY);
        return Some(pm);
    }
    let n = gs.len() as f32;
    let gap = if gs.len() >= 3 { 125.0 } else { 185.0 } * u;
    let tips = has_tips(data.product);
    let small = if gs.len() >= 3 { 0.82 } else { 1.0 };
    for (i, g) in gs.iter().enumerate() {
        let cx = w / 2.0 + (i as f32 - (n - 1.0) / 2.0) * gap;
        let cy = 160.0 * u;
        let s = u * small;
        let b = match g {
            Group::Pair(b) => {
                if !user_image(&mut pm, data.product, &["buds"], cx, cy, 160.0 * s, 130.0 * s) {
                    bud(&mut pm, cx - 30.0 * s, cy, s, -1.0, tips);
                    bud(&mut pm, cx + 30.0 * s, cy, s, 1.0, tips);
                }
                b
            }
            Group::Bud(b) => {
                let m = if b.component == Component::Left { -1.0 } else { 1.0 };
                let file = if m < 0.0 { "left" } else { "right" };
                if !user_image(&mut pm, data.product, &[file], cx, cy, 90.0 * s, 130.0 * s) {
                    bud(&mut pm, cx, cy, s, m, tips);
                    // Pictures usually show the printed L/R themselves; label only the drawing.
                    let side = if m < 0.0 { "L" } else { "R" };
                    text(&mut pm, &fonts.bold, side, 12.0 * u, cx + 34.0 * s * m, cy + 50.0 * s, GRAY);
                }
                b
            }
            Group::Case(b) => {
                if !user_image(&mut pm, data.product, &["case"], cx, cy + 6.0 * u, 150.0 * s, 120.0 * s) {
                    case(&mut pm, cx, cy + 6.0 * u, s, b.charge == Charge::Charging);
                }
                b
            }
            Group::Single(b) => {
                if !user_image(&mut pm, data.product, &["headphones", "buds"], cx, cy, 170.0 * s, 130.0 * s) {
                    headphones(&mut pm, cx, cy, s);
                }
                b
            }
        };
        battery(&mut pm, fonts, b, cx, 232.0 * u, u);
    }
    if let Some(state) = button {
        let mut pb = PathBuilder::new();
        rounded(&mut pb, BUTTON_X * u, BUTTON_Y * u, w - 2.0 * BUTTON_X * u, BUTTON_H * u, 14.0 * u);
        let (fill_c, text_c) = match state {
            Conn::Idle | Conn::Failed => (ACCENT, SHEET),
            Conn::Connecting => (CLOSE_BG, GRAY),
            Conn::Done => (GREEN, SHEET),
        };
        fill(&mut pm, pb, &paint(fill_c, 255));
        text(&mut pm, &fonts.bold, state.label(), 17.0 * u, w / 2.0, (BUTTON_Y + 30.5) * u, text_c);
    }
    Some(pm)
}

/// Renders the card to a PNG (for previews and tests without a compositor).
pub fn render_png(data: &PopupData, path: &str, scale: u32) -> Result<()> {
    let fonts = Fonts::load()?;
    let button = data.connect.as_ref().map(|_| Conn::Idle);
    let card = render_card(data, &fonts, scale as f32, button).context("render failed")?;
    card.save_png(path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Wayland plumbing

impl CompositorHandler for Popup {
    fn scale_factor_changed(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, f: i32) {
        if f != self.scale {
            self.scale = f;
            if self.configured {
                self.draw(qh);
            }
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.draw(qh);
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Popup {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Popup {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        if !self.configured {
            self.configured = true;
            self.start = Instant::now();
            self.draw(qh);
        }
    }
}

impl SeatHandler for Popup {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Popup {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            self.pointer_at = e.position;
            if matches!(e.kind, PointerEventKind::Press { .. }) {
                let (x, y) = (e.position.0 as f32, e.position.1 as f32);
                let on_button = (BUTTON_X..=W as f32 - BUTTON_X).contains(&x) && (BUTTON_Y..=BUTTON_Y + BUTTON_H).contains(&y);
                if let (true, Some(addr)) = (on_button, self.data.connect.clone()) {
                    let mut c = self.conn.lock().unwrap();
                    if matches!(*c, Conn::Idle | Conn::Failed) {
                        *c = Conn::Connecting;
                        connect(addr, self.conn.clone());
                    }
                    continue;
                }
                // Anywhere else: dismiss (start the fade-out now), unless a connection is running.
                if *self.conn.lock().unwrap() == Conn::Connecting {
                    continue;
                }
                let now = Instant::now();
                if self.hide_at.is_none_or(|t| t > now) {
                    self.hide_at = Some(now);
                }
            }
        }
    }
}

impl ShmHandler for Popup {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Popup);

impl ProvidesRegistryState for Popup {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(Popup);
