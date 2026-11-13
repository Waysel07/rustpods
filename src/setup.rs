//! Getting the background service running without a terminal.
//!
//! Packages ship `/etc/xdg/autostart/rustpods-daemon.desktop`. A file with the same name in
//! `~/.config/autostart/` overrides it (XDG autostart spec), which is how the settings window
//! turns "start at login" on or off, and how the AppImage (which ships no system files)
//! registers itself.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const NAME: &str = "rustpods-daemon.desktop";
const SYSTEM: &str = "/etc/xdg/autostart/rustpods-daemon.desktop";

fn user_file() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("autostart")
        .join(NAME)
}

/// The command that starts this very program: inside an AppImage that is the AppImage file,
/// not the temporary mount the binary runs from.
pub fn self_command() -> String {
    if let Some(a) = std::env::var_os("APPIMAGE") {
        return format!("\"{}\"", Path::new(&a).display());
    }
    match std::env::current_exe() {
        Ok(p) => format!("\"{}\"", p.display()),
        Err(_) => "rustpods".into(),
    }
}

fn systemd_service_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "rustpods.service"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn hidden(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim();
        l == "Hidden=true" || l == "X-GNOME-Autostart-enabled=false"
    })
}

/// Whether the service will start at the next login.
pub fn autostart_enabled() -> bool {
    if systemd_service_enabled() {
        return true;
    }
    match std::fs::read_to_string(user_file()) {
        Ok(t) => !hidden(&t),
        Err(_) => Path::new(SYSTEM).exists(),
    }
}

pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let f = user_file();
    if let Some(d) = f.parent() {
        std::fs::create_dir_all(d)?;
    }
    if !on && systemd_service_enabled() {
        // Installed with `rustpods install`: switch that off too.
        let _ = Command::new("systemctl").args(["--user", "disable", "rustpods.service"]).status();
    }
    let body = format!(
        "[Desktop Entry]\nType=Application\nName=RustPods background service\n\
         Comment=AirPods battery, ear detection, popup and stem presses\n\
         Exec={} daemon\nIcon=rustpods\nNoDisplay=true\nTerminal=false\n\
         X-GNOME-Autostart-enabled={on}\n{}",
        self_command(),
        if on { "" } else { "Hidden=true\n" }
    );
    std::fs::write(f, body)
}

/// Starts the daemon in the background, detached from this process.
pub fn start_daemon() -> std::io::Result<()> {
    if systemd_service_enabled() {
        let st = Command::new("systemctl").args(["--user", "start", "rustpods.service"]).status()?;
        if st.success() {
            return Ok(());
        }
    }
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // New session so closing the settings window doesn't take the daemon with it.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    std::thread::spawn(move || {
        let mut c = child;
        let _ = c.wait();
    });
    Ok(())
}

/// First launch of the app: make sure it keeps working after this window closes.
/// Only does anything the first time (or when nothing was ever set up).
pub fn first_run() {
    let marker = crate::config::Config::path().with_file_name("setup-done");
    if marker.exists() {
        return;
    }
    if !autostart_enabled() {
        let _ = set_autostart(true);
    }
    if let Some(d) = marker.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(marker, "");
}

#[cfg(test)]
mod tests {
    #[test]
    fn hidden_detection() {
        assert!(super::hidden("[Desktop Entry]\nHidden=true\n"));
        assert!(super::hidden("X-GNOME-Autostart-enabled=false"));
        assert!(!super::hidden("[Desktop Entry]\nExec=x daemon\nX-GNOME-Autostart-enabled=true\n"));
    }
}
