//! Equalizer: a PipeWire filter-chain that sits in front of the AirPods only.
//!
//! The daemon runs `pipewire -c <conf>` with a filter-chain made of PipeWire's built-in
//! biquads. It is a WirePlumber *smart filter* targeting the AirPods' A2DP sink, so the AirPods
//! stay the default output: volume keys, the desktop OSD and ducking keep working on the real
//! device, and the laptop speakers are never touched. Changing gains is done live with
//! `pw-cli set-param`, so moving a slider doesn't interrupt the music.

use crate::config::{EqConfig, EqPreset};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    LowShelf,
    Peak,
    HighShelf,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::LowShelf => "bq_lowshelf",
            Kind::Peak => "bq_peaking",
            Kind::HighShelf => "bq_highshelf",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: Kind,
    pub freq: f32,
    pub gain: f32,
    pub q: f32,
}

const fn ls(freq: f32, gain: f32, q: f32) -> Band {
    Band { kind: Kind::LowShelf, freq, gain, q }
}
const fn pk(freq: f32, gain: f32, q: f32) -> Band {
    Band { kind: Kind::Peak, freq, gain, q }
}
const fn hs(freq: f32, gain: f32, q: f32) -> Band {
    Band { kind: Kind::HighShelf, freq, gain, q }
}

/// A measured correction curve towards the AutoEQ (Harman-style) target.
pub struct Tuning {
    pub source: &'static str,
    pub bands: &'static [Band],
}

// From AutoEQ (github.com/jaakkopasanen/AutoEq), results/Rtings/*/ParametricEQ.txt.
// The preamp is recomputed from the final curve, so it isn't stored here.
const AIRPODS_1: Tuning = Tuning {
    source: "Rtings (HMS II.3)",
    bands: &[
        ls(105.0, 6.2, 0.70), pk(8916.0, -4.0, 1.30), pk(985.0, 2.4, 0.95), pk(1873.0, -4.1, 1.27),
        pk(401.0, 0.9, 0.34), hs(10000.0, -3.0, 0.70), pk(4273.0, 1.4, 2.96), pk(75.0, 0.8, 3.69),
        pk(5606.0, -1.8, 6.00), pk(8681.0, 1.3, 2.83),
    ],
};
const AIRPODS_2: Tuning = Tuning {
    source: "Rtings (HMS II.3)",
    bands: &[
        ls(105.0, 5.8, 0.70), pk(5446.0, -6.5, 0.18), pk(2138.0, 8.3, 0.30), pk(89.0, 1.5, 0.98),
        pk(2124.0, -6.3, 1.11), hs(10000.0, -2.5, 0.70), pk(4173.0, 1.4, 2.74), pk(5484.0, -1.6, 6.00),
        pk(2846.0, -0.9, 4.81), pk(240.0, 0.2, 1.68),
    ],
};
const AIRPODS_3: Tuning = Tuning {
    source: "Rtings (B&K 5128)",
    bands: &[
        ls(105.0, 6.9, 0.70), pk(1838.0, -3.9, 2.08), pk(3879.0, 2.5, 2.13), pk(64.0, -5.0, 0.96),
        pk(43.0, 2.7, 3.38), hs(10000.0, 5.5, 0.70), pk(9961.0, 2.4, 1.61), pk(6052.0, -2.7, 3.91),
        pk(724.0, 1.9, 2.80), pk(420.0, -0.6, 1.89),
    ],
};
const AIRPODS_4: Tuning = Tuning {
    source: "Rtings (B&K 5128)",
    bands: &[
        ls(105.0, 11.4, 0.70), pk(4622.0, 6.0, 1.39), pk(49.0, -11.7, 0.39), pk(1277.0, -2.6, 0.86),
        pk(3036.0, 4.1, 2.43), hs(10000.0, -1.9, 0.70), pk(354.0, -1.4, 1.50), pk(176.0, 1.5, 2.04),
        pk(618.0, 1.0, 2.35), pk(106.0, -0.9, 2.47),
    ],
};
const AIRPODS_4_ANC: Tuning = Tuning {
    source: "Rtings (B&K 5128, ANC on)",
    bands: &[
        ls(105.0, 12.7, 0.70), pk(49.0, -13.2, 0.44), pk(3573.0, 6.0, 1.40), pk(1318.0, -2.8, 1.16),
        pk(5138.0, 3.2, 2.82), hs(10000.0, -2.0, 0.70), pk(166.0, 1.2, 3.49), pk(100.0, -0.7, 2.50),
        pk(438.0, -0.8, 2.38), pk(654.0, 0.6, 2.93),
    ],
};
const AIRPODS_PRO: Tuning = Tuning {
    source: "Rtings (HMS II.3)",
    bands: &[
        ls(105.0, 3.2, 0.70), pk(436.0, -2.6, 0.90), pk(4595.0, 2.6, 2.54), pk(918.0, 2.4, 2.14),
        pk(142.0, 0.7, 1.41), hs(10000.0, -7.0, 0.70), pk(48.0, -0.4, 1.62), pk(2445.0, 0.8, 2.39),
        pk(1548.0, -0.5, 3.19), pk(5376.0, 0.8, 6.00),
    ],
};
const AIRPODS_PRO_2: Tuning = Tuning {
    source: "Rtings (B&K 5128)",
    bands: &[
        ls(105.0, -1.5, 0.70), pk(377.0, -2.5, 0.74), pk(3442.0, 3.0, 0.67), pk(6303.0, -3.4, 4.47),
        pk(1339.0, 0.5, 2.65), hs(10000.0, 3.1, 0.70), pk(82.0, 1.0, 1.92), pk(170.0, -0.6, 1.86),
        pk(8133.0, -1.1, 2.86), pk(311.0, 0.4, 3.57),
    ],
};
const AIRPODS_MAX: Tuning = Tuning {
    source: "Rtings (B&K 5128)",
    bands: &[
        ls(105.0, -2.1, 0.70), pk(5092.0, 5.9, 1.63), pk(414.0, -1.7, 0.23), pk(9875.0, -3.4, 2.73),
        pk(1895.0, 2.9, 2.72), hs(10000.0, 0.0, 0.70), pk(207.0, -1.3, 1.30), pk(365.0, 1.2, 2.60),
        pk(91.0, 1.0, 1.98), pk(621.0, 0.7, 2.98),
    ],
};

/// The measured correction for a model, if AutoEQ has one.
pub fn tuning(product: u16) -> Option<&'static Tuning> {
    Some(match product {
        0x2002 => &AIRPODS_1,
        0x200f => &AIRPODS_2,
        0x2013 => &AIRPODS_3,
        0x2019 => &AIRPODS_4,
        0x201b => &AIRPODS_4_ANC,
        0x200e => &AIRPODS_PRO,
        0x2014 | 0x2024 => &AIRPODS_PRO_2,
        0x200a | 0x201f => &AIRPODS_MAX,
        _ => return None,
    })
}

pub const BASS_FREQ: f32 = 105.0;
pub const TREBLE_FREQ: f32 = 8000.0;

/// (use the model correction, bass dB, treble dB) for a preset.
pub fn preset_values(cfg: &EqConfig) -> (bool, f32, f32) {
    match cfg.preset {
        EqPreset::Neutral => (true, 0.0, 0.0),
        // The neutral curve already lifts the bass to the target; this adds a club-style
        // shelf on top without turning the small drivers into mud.
        EqPreset::BassBoost => (true, 4.0, 0.0),
        // Relaxed: a little more body, softer highs for long listening.
        EqPreset::Warm => (true, 2.0, -2.5),
        EqPreset::Custom => (cfg.correction, cfg.bass.clamp(-12.0, 12.0), cfg.treble.clamp(-12.0, 12.0)),
    }
}

/// The full curve. The layout depends only on the model (tuning bands + bass + treble), so
/// every settings change can be applied live.
pub fn chain(cfg: &EqConfig, product: u16) -> Chain {
    let (correct, bass, treble) = preset_values(cfg);
    let mut bands: Vec<Band> = tuning(product)
        .map(|t| t.bands.iter().map(|b| Band { gain: if correct { b.gain } else { 0.0 }, ..*b }).collect())
        .unwrap_or_default();
    bands.push(ls(BASS_FREQ, bass, 0.7));
    bands.push(hs(TREBLE_FREQ, treble, 0.7));
    let preamp = -(peak_db(&bands).max(0.0) * 10.0).ceil() / 10.0;
    Chain { preamp, bands }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chain {
    pub preamp: f32,
    pub bands: Vec<Band>,
}

// ---------------------------------------------------------------------------
// Response (to pick a preamp that never clips)

const RATE: f64 = 48000.0;

/// RBJ cookbook coefficients (b0, b1, b2, a0, a1, a2), as PipeWire's built-in biquads use.
fn coeffs(b: &Band) -> [f64; 6] {
    let a = 10f64.powf(b.gain as f64 / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * b.freq as f64 / RATE;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * b.q as f64);
    match b.kind {
        Kind::Peak => [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
        Kind::LowShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            [
                a * ((a + 1.0) - (a - 1.0) * cos + s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - s),
                (a + 1.0) + (a - 1.0) * cos + s,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - s,
            ]
        }
        Kind::HighShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            [
                a * ((a + 1.0) + (a - 1.0) * cos + s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - s),
                (a + 1.0) - (a - 1.0) * cos + s,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - s,
            ]
        }
    }
}

/// Gain of the whole chain at a frequency, in dB.
pub fn response_db(bands: &[Band], freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / RATE;
    let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
    bands
        .iter()
        .map(|b| {
            let [b0, b1, b2, a0, a1, a2] = coeffs(b);
            let num = (b0 + b1 * c1 + b2 * c2).hypot(b1 * s1 + b2 * s2);
            let den = (a0 + a1 * c1 + a2 * c2).hypot(a1 * s1 + a2 * s2);
            20.0 * (num / den).log10()
        })
        .sum()
}

fn peak_db(bands: &[Band]) -> f32 {
    (0..=240)
        .map(|i| 20.0 * 1000f64.powf(i as f64 / 240.0)) // 20 Hz … 20 kHz, log-spaced
        .map(|f| response_db(bands, f))
        .fold(f64::MIN, f64::max) as f32
}

// ---------------------------------------------------------------------------
// PipeWire

pub const NODE: &str = "rustpods-eq";

/// The AirPods' sink, but only while it plays music (A2DP); calls stay untouched.
pub fn a2dp_sink(address: &str) -> Option<String> {
    let out = Command::new("pactl").args(["-f", "json", "list", "sinks"]).output().ok()?;
    let sinks: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    sinks.as_array()?.iter().find_map(|s| {
        let p = &s["properties"];
        let ok = p["api.bluez5.address"].as_str()?.eq_ignore_ascii_case(address)
            && p["api.bluez5.profile"].as_str()?.starts_with("a2dp");
        ok.then(|| s["name"].as_str().map(String::from)).flatten()
    })
}

fn name(i: usize) -> String {
    format!("b{i}")
}

/// SPA-JSON strings: quote and escape.
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

pub fn conf(chain: &Chain, sink: &str) -> String {
    let mut nodes = format!(
        "          {{ type = builtin name = pre label = bq_highshelf control = {{ \"Freq\" = 0.0 \"Q\" = 1.0 \"Gain\" = {:.2} }} }}\n",
        chain.preamp
    );
    let mut links = String::new();
    let mut prev = "pre".to_string();
    for (i, b) in chain.bands.iter().enumerate() {
        nodes += &format!(
            "          {{ type = builtin name = {} label = {} control = {{ \"Freq\" = {:.1} \"Q\" = {:.3} \"Gain\" = {:.2} }} }}\n",
            name(i),
            b.kind.label(),
            b.freq,
            b.q,
            b.gain
        );
        links += &format!("          {{ output = \"{prev}:Out\" input = \"{}:In\" }}\n", name(i));
        prev = name(i);
    }
    format!(
        r#"# Written by rustpods; regenerated whenever the equalizer starts.
context.properties = {{ log.level = 2 }}
context.spa-libs = {{
  audio.convert.* = audioconvert/libspa-audioconvert
  support.*       = support/libspa-support
}}
context.modules = [
  {{ name = libpipewire-module-rt flags = [ ifexists nofail ] }}
  {{ name = libpipewire-module-protocol-native }}
  {{ name = libpipewire-module-client-node }}
  {{ name = libpipewire-module-adapter }}
  {{ name = libpipewire-module-filter-chain
    args = {{
      node.description = "AirPods Equalizer"
      media.name = "AirPods Equalizer"
      filter.graph = {{
        nodes = [
{nodes}        ]
        links = [
{links}        ]
      }}
      audio.channels = 2
      audio.position = [ FL FR ]
      capture.props = {{
        node.name = {node}
        media.class = Audio/Sink
        filter.smart = true
        filter.smart.name = {node}
        filter.smart.target = {{ node.name = {sink} }}
      }}
      playback.props = {{
        node.name = {out}
        node.passive = true
        node.dont-fallback = true
        node.dont-reconnect = true
      }}
    }}
  }}
]
"#,
        node = q(NODE),
        out = q(&format!("{NODE}.output")),
        sink = q(sink),
    )
}

/// `pw-cli set-param` argument that sets every control of the chain at once.
pub fn props(chain: &Chain) -> String {
    let mut p = format!("\"pre:Gain\" {:.2}", chain.preamp);
    for (i, b) in chain.bands.iter().enumerate() {
        p += &format!(" \"{0}:Freq\" {1:.1} \"{0}:Q\" {2:.3} \"{0}:Gain\" {3:.2}", name(i), b.freq, b.q, b.gain);
    }
    format!("{{ params = [ {p} ] }}")
}

fn conf_path() -> PathBuf {
    crate::daemon::runtime_dir().join("rustpods-eq.conf")
}

/// The running filter, owned by one AirPods session.
#[derive(Default)]
pub struct Filter {
    child: Option<Child>,
    sink: String,
    applied: Option<Chain>,
    retry_at: Option<Instant>,
}

impl Filter {
    /// Bring the filter in line with the settings: start, update live, or stop.
    pub fn sync(&mut self, cfg: &EqConfig, product: u16, address: &str) {
        if let Some(Ok(Some(st))) = self.child.as_mut().map(|c| c.try_wait()) {
            eprintln!("equalizer: filter exited ({st}); retrying in 30 s");
            self.child = None;
            self.applied = None;
            self.retry_at = Some(Instant::now() + Duration::from_secs(30));
        }
        let sink = if cfg.enabled { a2dp_sink(address) } else { None };
        let Some(sink) = sink else {
            self.stop();
            return;
        };
        let want = chain(cfg, product);
        if self.child.is_some() && self.sink == sink {
            let same_layout = self.applied.as_ref().is_some_and(|a| a.bands.len() == want.bands.len());
            if self.applied.as_ref() == Some(&want) {
                return;
            }
            if same_layout && set_live(&want) {
                self.applied = Some(want);
                return;
            }
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.stop();
        match self.start(&want, &sink) {
            Ok(c) => {
                eprintln!("equalizer: on ({:?}, preamp {:.1} dB) -> {sink}", cfg.preset, want.preamp);
                self.child = Some(c);
                self.sink = sink;
                self.applied = Some(want);
                self.retry_at = None;
            }
            Err(e) => {
                eprintln!("equalizer: {e}; retrying in 30 s");
                self.retry_at = Some(Instant::now() + Duration::from_secs(30));
            }
        }
    }

    fn start(&self, chain: &Chain, sink: &str) -> anyhow::Result<Child> {
        let path = conf_path();
        std::fs::write(&path, conf(chain, sink))?;
        let mut cmd = Command::new("pipewire");
        cmd.arg("-c").arg(&path).stdin(Stdio::null()).stdout(Stdio::null());
        // If the daemon dies without cleaning up, take the filter with it. (The death signal
        // follows the spawning thread; this runs on a long-lived tokio worker.)
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        Ok(cmd.spawn()?)
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            unsafe {
                libc::kill(c.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while matches!(c.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = c.kill();
            let _ = c.wait();
            eprintln!("equalizer: off");
        }
        self.applied = None;
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn set_live(chain: &Chain) -> bool {
    Command::new("pw-cli")
        .args(["set-param", NODE, "Props", &props(chain)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(preset: EqPreset) -> EqConfig {
        EqConfig { enabled: true, preset, ..Default::default() }
    }

    #[test]
    fn neutral_preamp_matches_autoeq() {
        // AutoEQ's own preamp for AirPods 3 is -6.4 dB; ours is computed from the curve.
        let c = chain(&cfg(EqPreset::Neutral), 0x2013);
        assert!((-7.5..=-5.5).contains(&c.preamp), "preamp {}", c.preamp);
        assert_eq!(c.bands.len(), 12);
    }

    #[test]
    fn never_boosts_above_zero() {
        for p in [EqPreset::Neutral, EqPreset::BassBoost, EqPreset::Warm] {
            for product in [0x2002, 0x200f, 0x2013, 0x2019, 0x201b, 0x200e, 0x2014, 0x200a, 0x2027] {
                let c = chain(&cfg(p), product);
                let peak = peak_db(&c.bands) + c.preamp;
                assert!(peak <= 0.01, "{p:?} {product:#x}: {peak}");
            }
        }
    }

    #[test]
    fn flat_is_flat() {
        let c = chain(&EqConfig { preset: EqPreset::Custom, correction: false, ..cfg(EqPreset::Custom) }, 0x2013);
        assert_eq!(c.preamp, 0.0);
        assert!(response_db(&c.bands, 1000.0).abs() < 1e-6);
    }

    #[test]
    fn bass_boost_adds_bass() {
        let n = chain(&cfg(EqPreset::Neutral), 0x2013);
        let b = chain(&cfg(EqPreset::BassBoost), 0x2013);
        let lift = response_db(&b.bands, 50.0) - response_db(&n.bands, 50.0);
        assert!((3.5..=4.5).contains(&lift), "lift {lift}");
        assert_eq!(n.bands.len(), b.bands.len(), "same layout so it can change live");
    }

    #[test]
    fn conf_is_wellformed() {
        let c = chain(&cfg(EqPreset::Neutral), 0x2013);
        let s = conf(&c, "bluez_output.34_28_40_D8_8A_D4.1");
        assert!(s.contains("filter.smart.target = { node.name = \"bluez_output.34_28_40_D8_8A_D4.1\" }"));
        assert_eq!(s.matches("type = builtin").count(), 13);
        assert_eq!(s.matches("output = ").count(), 12);
        assert!(props(&c).contains("\"b11:Gain\""));
    }
}
