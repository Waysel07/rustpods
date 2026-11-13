//! `rustpods settings`: a small window for the config file, with live status from the daemon.

use crate::aap::{self, Charge, Component, NoiseMode};
use crate::config::{Action, Config, Press};
use crate::daemon::{self, State};
use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const GREEN: Color32 = Color32::from_rgb(52, 199, 89);
const RED: Color32 = Color32::from_rgb(255, 69, 58);
const ORANGE: Color32 = Color32::from_rgb(255, 159, 10);
const BLUE: Color32 = Color32::from_rgb(10, 132, 255);

/// What the background thread learned from the daemon.
#[derive(Clone, Default)]
enum Link {
    #[default]
    Unknown,
    NoDaemon,
    Status(State),
}

/// Blocking one-line request to the daemon (the window has no async runtime of its own).
fn ask(line: &str) -> Option<Result<State, String>> {
    let mut s = UnixStream::connect(daemon::socket_path()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    s.write_all(format!("{line}\n").as_bytes()).ok()?;
    let mut out = String::new();
    BufReader::new(s).read_line(&mut out).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    if v["ok"].as_bool() == Some(true) {
        Some(serde_json::from_value(v["state"].clone()).map_err(|e| e.to_string()))
    } else {
        Some(Err(v["error"].as_str().unwrap_or("error").to_string()))
    }
}

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustPods")
            .with_app_id("rustpods")
            .with_inner_size([460.0, 720.0])
            .with_min_inner_size([380.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("RustPods", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
        .map_err(|e| anyhow::anyhow!("could not open the settings window: {e}"))
}

struct App {
    cfg: Config,
    autostart: bool,
    link: Arc<Mutex<Link>>,
    left: bool,
    note: Option<(String, bool)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> App {
        let mut style = (*cc.egui_ctx.global_style()).clone();
        use egui::{FontFamily::Proportional, FontId, TextStyle};
        style.text_styles = [
            (TextStyle::Heading, FontId::new(22.0, Proportional)),
            (TextStyle::Body, FontId::new(15.0, Proportional)),
            (TextStyle::Button, FontId::new(14.5, Proportional)),
            (TextStyle::Small, FontId::new(12.5, Proportional)),
            (TextStyle::Monospace, FontId::new(13.5, egui::FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style.spacing.combo_width = 190.0;
        cc.egui_ctx.set_global_style(style);

        let link = Arc::new(Mutex::new(Link::Unknown));
        let (l2, ctx) = (link.clone(), cc.egui_ctx.clone());
        std::thread::spawn(move || {
            loop {
                let next = match ask("status") {
                    None => Link::NoDaemon,
                    Some(Ok(st)) => Link::Status(st),
                    Some(Err(_)) => Link::NoDaemon,
                };
                *l2.lock().unwrap() = next;
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        // Opening the app is all the setup there is: start the service if it isn't running.
        crate::setup::first_run();
        if ask("status").is_none() {
            let _ = crate::setup::start_daemon();
        }
        App { cfg: Config::load(), autostart: crate::setup::autostart_enabled(), link, left: true, note: None }
    }

    fn save(&mut self) {
        self.note = Some(match self.cfg.save() {
            Ok(()) => match ask("reload") {
                Some(_) => ("Saved. Active now.".into(), false),
                None => ("Saved. Start the service to use it.".into(), false),
            },
            Err(e) => (format!("Could not save: {e}"), true),
        });
    }
}

// ---------------------------------------------------------------------------
// Small widgets

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let fill = if ui.visuals().dark_mode { Color32::from_gray(38) } else { Color32::WHITE };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(14))
        .stroke(Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color))
        .inner_margin(14)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(title.to_uppercase()).small().strong().color(ui.visuals().weak_text_color()));
    ui.add_space(2.0);
}

/// iOS-style switch.
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = Vec2::new(44.0, 26.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off_fill = if ui.visuals().dark_mode { Color32::from_gray(70) } else { Color32::from_gray(205) };
    let fill = lerp_color(off_fill, GREEN, t);
    ui.painter().rect_filled(rect, CornerRadius::same(13), fill);
    let x = egui::lerp(rect.left() + 13.0..=rect.right() - 13.0, t);
    ui.painter().circle(egui::pos2(x, rect.center().y), 11.0, Color32::WHITE, Stroke::new(0.5, Color32::from_black_alpha(40)));
    resp
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// A row with a title, an optional grey subtitle, and a widget on the right.
fn row<R>(ui: &mut egui::Ui, title: &str, sub: Option<&str>, right: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(title);
            if let Some(s) = sub {
                ui.label(RichText::new(s).small().color(ui.visuals().weak_text_color()));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), right).inner
    })
    .inner
}

fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (v, label) in options {
            let selected = *value == *v;
            let text = if selected { RichText::new(*label).strong().color(Color32::WHITE) } else { RichText::new(*label) };
            let b = egui::Button::new(text).corner_radius(CornerRadius::same(8)).fill(if selected {
                BLUE
            } else {
                ui.visuals().widgets.inactive.weak_bg_fill
            });
            if ui.add(b).clicked() && !selected {
                *value = *v;
                changed = true;
            }
        }
    });
    changed
}

fn battery(ui: &mut egui::Ui, label: &str, b: Option<&aap::Battery>) {
    ui.vertical(|ui| {
        ui.set_width(84.0);
        ui.label(RichText::new(label).small().color(ui.visuals().weak_text_color()));
        match b.filter(|b| b.charge != Charge::Disconnected) {
            None => {
                ui.label(RichText::new("–").size(20.0).strong());
            }
            Some(b) => {
                let color = if b.charge == Charge::Charging {
                    GREEN
                } else if b.level <= 20 {
                    RED
                } else {
                    ui.visuals().strong_text_color()
                };
                let bolt = if b.charge == Charge::Charging { " ⚡" } else { "" };
                ui.label(RichText::new(format!("{}%{bolt}", b.level)).size(20.0).strong().color(color));
                let (rect, _) = ui.allocate_exact_size(Vec2::new(64.0, 5.0), Sense::hover());
                ui.painter().rect_filled(rect, CornerRadius::same(3), ui.visuals().widgets.inactive.weak_bg_fill);
                let mut f = rect;
                f.set_width(rect.width() * b.level as f32 / 100.0);
                ui.painter().rect_filled(f, CornerRadius::same(3), color);
            }
        }
    });
}

// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let link = self.link.lock().unwrap().clone();
        let state = match &link {
            Link::Status(s) if s.connected => Some(s.clone()),
            _ => None,
        };
        let bg = if ui.visuals().dark_mode { Color32::from_gray(24) } else { Color32::from_rgb(242, 242, 247) };
        egui::CentralPanel::default().frame(egui::Frame::new().fill(bg).inner_margin(18)).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.header(ui, &link, state.as_ref());
                if let Some(st) = &state {
                    if !st.noise_modes.is_empty() {
                        self.noise(ui, st);
                    }
                }
                self.presses(ui, state.as_ref());
                self.automatic(ui);
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(state.is_some(), egui::Button::new("Show battery popup")).clicked() {
                        let _ = ask("popup");
                    }
                    if let Some((msg, err)) = &self.note {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(msg).small().color(if *err { RED } else { ui.visuals().weak_text_color() }));
                        });
                    }
                });
            });
        });
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui, link: &Link, st: Option<&State>) {
        card(ui, |ui| {
            let (title, sub, dot) = match (link, st) {
                (_, Some(s)) => (s.name.clone(), s.model.clone(), GREEN),
                (Link::Status(_), None) => ("AirPods".into(), "Not connected. Open the case near your computer.".into(), ORANGE),
                (Link::NoDaemon, _) => ("RustPods".into(), "The background service isn't running.".into(), RED),
                (Link::Unknown, _) => ("RustPods".into(), "Looking for the service…".into(), Color32::GRAY),
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 5.0, dot);
                ui.label(RichText::new(title).heading().strong());
            });
            ui.label(RichText::new(sub).color(ui.visuals().weak_text_color()));
            if matches!(link, Link::NoDaemon) {
                ui.add_space(6.0);
                if ui.button("Start RustPods").clicked() {
                    self.note = Some(match crate::setup::start_daemon() {
                        Ok(()) => ("Starting…".into(), false),
                        Err(e) => (format!("Could not start: {e}"), true),
                    });
                }
            }
            if let Some(s) = st {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let get = |c| s.batteries.iter().find(|b| b.component == c);
                    if let Some(b) = get(Component::Single) {
                        battery(ui, "Battery", Some(b));
                    } else {
                        battery(ui, "Left", get(Component::Left));
                        battery(ui, "Right", get(Component::Right));
                        battery(ui, "Case", get(Component::Case));
                    }
                });
            }
        });
    }

    fn noise(&mut self, ui: &mut egui::Ui, st: &State) {
        section(ui, "Noise control");
        card(ui, |ui| {
            let mut mode = st.noise.unwrap_or(NoiseMode::Off);
            let opts: Vec<(NoiseMode, &str)> = st
                .noise_modes
                .iter()
                .map(|m| {
                    (*m, match m {
                        NoiseMode::Off => "Off",
                        NoiseMode::Anc => "Noise cancellation",
                        NoiseMode::Transparency => "Transparency",
                        NoiseMode::Adaptive => "Adaptive",
                    })
                })
                .collect();
            if segmented(ui, &mut mode, &opts) {
                let word = match mode {
                    NoiseMode::Off => "off",
                    NoiseMode::Anc => "anc",
                    NoiseMode::Transparency => "transparency",
                    NoiseMode::Adaptive => "adaptive",
                };
                let _ = ask(&format!("noise {word}"));
            }
            if aap::has_conversation_awareness(st.product) {
                ui.separator();
                let mut on = st.conversation.unwrap_or(false);
                row(ui, "Conversation awareness", Some("Lowers the media and lets voices in while you talk"), |ui| {
                    if toggle(ui, &mut on).changed() {
                        let _ = ask(&format!("conversation {}", if on { "on" } else { "off" }));
                    }
                });
            }
        });
    }

    fn presses(&mut self, ui: &mut egui::Ui, st: Option<&State>) {
        section(ui, "Stem presses");
        let mut changed = false;
        card(ui, |ui| {
            if let Some(s) = st.filter(|s| !aap::has_stem_presses(s.product)) {
                ui.label(RichText::new(format!("{} have no press sensor, so these settings don't apply.", s.model)).color(ORANGE));
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                segmented(ui, &mut self.left, &[(true, "Left AirPod"), (false, "Right AirPod")]);
            });
            ui.add_space(6.0);
            let side_name = if self.left { "left" } else { "right" };
            for press in Press::ALL {
                ui.separator();
                let id = format!("{side_name}-{press:?}");
                let sub = format!("Default: {}", press.default_label());
                let side = if self.left { &mut self.cfg.left } else { &mut self.cfg.right };
                let action = side.get_mut(press);
                row(ui, press.label(), Some(&sub), |ui| {
                    egui::ComboBox::from_id_salt(&id).selected_text(action.label()).show_ui(ui, |ui| {
                        for choice in Action::CHOICES {
                            let label = choice.label();
                            if ui.selectable_value(action, choice, label).changed() {
                                changed = true;
                            }
                        }
                        let is_cmd = matches!(action, Action::Command(_));
                        if ui.selectable_label(is_cmd, Action::Command(String::new()).label()).clicked() && !is_cmd {
                            *action = Action::Command(String::new());
                            changed = true;
                        }
                    });
                });
                if let Action::Command(cmd) = action {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Command").small());
                        let edit = egui::TextEdit::singleline(cmd)
                            .hint_text("e.g. playerctl next, or notify-send Hi")
                            .desired_width(f32::INFINITY);
                        if ui.add(edit).lost_focus() {
                            changed = true;
                        }
                    });
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                let other = if self.left { "right" } else { "left" };
                if ui.button(format!("Copy to {other} AirPod")).clicked() {
                    if self.left {
                        self.cfg.right = self.cfg.left.clone();
                    } else {
                        self.cfg.left = self.cfg.right.clone();
                    }
                    changed = true;
                }
                if ui.button("Reset both").clicked() {
                    self.cfg.left = Default::default();
                    self.cfg.right = Default::default();
                    changed = true;
                }
            });
            ui.label(
                RichText::new(
                    "\"Default\" leaves a press to the AirPods. Once a press is changed on one side, \
                     RustPods handles it on both sides, so the service has to be running.",
                )
                .small()
                .color(ui.visuals().weak_text_color()),
            );
        });
        if changed {
            self.save();
        }
    }

    fn automatic(&mut self, ui: &mut egui::Ui) {
        section(ui, "Automatic");
        let mut changed = false;
        card(ui, |ui| {
            row(ui, "Start at login", Some("Runs in the background, no window needed"), |ui| {
                let mut on = self.autostart;
                if toggle(ui, &mut on).changed() {
                    match crate::setup::set_autostart(on) {
                        Ok(()) => self.autostart = on,
                        Err(e) => self.note = Some((format!("Could not change autostart: {e}"), true)),
                    }
                }
            });
            ui.separator();
            let c = &mut self.cfg;
            let items: [(&str, Option<&str>, &mut bool); 5] = [
                ("Pause when an AirPod comes out", Some("And resume when you put it back"), &mut c.ear_pause),
                ("Battery popup when connecting", None, &mut c.popup),
                ("Low battery notifications", Some("At 20 % and 10 %"), &mut c.notify_low),
                ("Battery in desktop widgets", Some("Applies after the service restarts"), &mut c.battery_provider),
                ("Presses control the playing app", Some("Applies after the service restarts"), &mut c.media_keys),
            ];
            for (i, (title, sub, v)) in items.into_iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                row(ui, title, sub, |ui| changed |= toggle(ui, v).changed());
            }
            ui.separator();
            row(ui, "Lower the volume while I talk", Some("AirPods with conversation awareness"), |ui| {
                changed |= toggle(ui, &mut c.duck).changed();
            });
            if c.duck {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Volume while talking").small());
                    let mut pct = (c.duck_level * 100.0).round();
                    let s = ui.add(egui::Slider::new(&mut pct, 0.0..=80.0).suffix(" %").step_by(5.0));
                    if s.changed() {
                        c.duck_level = pct / 100.0;
                    }
                    if s.drag_stopped() || (s.changed() && !s.dragged()) {
                        changed = true;
                    }
                });
            }
        });
        if changed {
            self.save();
        }
    }
}
