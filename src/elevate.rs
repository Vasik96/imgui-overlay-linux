//! Re-launches the program as root through the desktop's graphical authentication dialog
//! (polkit, i.e. `pkexec`) when it was started as a normal user, e.g. by double-clicking it.
//! Falls back to `sudo -E` when started from a terminal and `pkexec` is missing.
//!
//! `pkexec` wipes the environment, so the variables the overlay needs to reach the display
//! (DISPLAY / XAUTHORITY / ...) and its own OVERLAY_* settings are passed explicitly.

use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Result};

const FORWARD_ENV: &[&str] = &[
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
];

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path)
        .chain(["/usr/bin", "/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn kv(k: &str, v: &std::ffi::OsStr) -> OsString {
    let mut s = OsString::from(k);
    s.push("=");
    s.push(v);
    s
}

/// Returns `Ok(())` when already root. Otherwise replaces the process with the elevated
/// copy and never returns (or returns an error if that was impossible).
pub fn ensure_root() -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Ok(());
    }
    if env::var_os("OVERLAY_ELEVATED").is_some() {
        eprintln!("warning: elevation did not give root privileges; input devices may be unavailable");
        return Ok(());
    }

    let exe = env::current_exe()?;
    let args: Vec<OsString> = env::args_os().skip(1).collect();

    let mut vars: Vec<OsString> = vec![kv("OVERLAY_ELEVATED", "1".as_ref())];
    for k in FORWARD_ENV {
        if let Some(v) = env::var_os(k) {
            vars.push(kv(k, &v));
        }
    }
    for (k, v) in env::vars_os() {
        if let Some(k) = k.to_str() {
            if k.starts_with("OVERLAY_") && k != "OVERLAY_ELEVATED" || k.starts_with("VK_") {
                vars.push(kv(k, &v));
            }
        }
    }
    // Root's own home has no .Xauthority: point at the user's.
    if env::var_os("XAUTHORITY").is_none() {
        if let Some(home) = env::var_os("HOME") {
            let p = PathBuf::from(home).join(".Xauthority");
            if p.is_file() {
                vars.push(kv("XAUTHORITY", p.as_os_str()));
            }
        }
    }

    if let Some(pkexec) = find_in_path("pkexec") {
        eprintln!("not root: asking for authentication (pkexec)...");
        let err = Command::new(pkexec)
            .arg("env")
            .args(&vars)
            .arg(&exe)
            .args(&args)
            .exec(); // only returns on failure
        eprintln!("pkexec failed to start: {err}");
    }

    if unsafe { libc::isatty(0) } == 1 {
        if let Some(sudo) = find_in_path("sudo") {
            eprintln!("not root: re-running with sudo...");
            let err = Command::new(sudo)
                .arg("env")
                .args(&vars)
                .arg(&exe)
                .args(&args)
                .exec();
            eprintln!("sudo failed to start: {err}");
        }
    }

    bail!("root privileges are required to read /dev/input; install polkit (pkexec) or run with sudo")
}

/// When started from a file manager there is no terminal, so every message (including
/// errors) would vanish. Send stdout/stderr to `/tmp/imgui-x11-overlay-<uid>.log` instead.
/// Does nothing in a terminal, and in the elevated copy (which inherits the redirected fds).
pub fn redirect_output_if_no_tty() {
    use std::os::fd::AsRawFd;
    if env::var_os("OVERLAY_ELEVATED").is_some() || unsafe { libc::isatty(2) } == 1 {
        return;
    }
    let path = format!("/tmp/imgui-x11-overlay-{}.log", unsafe { libc::getuid() });
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        unsafe {
            libc::dup2(f.as_raw_fd(), 1);
            libc::dup2(f.as_raw_fd(), 2);
        }
        eprintln!("log: {path}");
    }
}
