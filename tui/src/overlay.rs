//! Out-of-sandbox fuse-overlay mount listener + open/overlay helpers.

use crate::paths::{cache_dir, expand_path, setting, staging_dir, steam_root};
use crate::steam::game_install_dir;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Where `bin` resolves on this shell's PATH, if anywhere. (Steam may
/// see a different PATH, so this is a best-effort signal.)
pub(crate) fn on_path(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Whether any Steam user's launch options mention twwh3-run. Steam
/// keeps launch options in userdata/<id>/config/localconfig.vdf.
/// None = could not read any config (unknown).
pub(crate) fn launch_options_use_shim() -> Option<bool> {
    let rd = fs::read_dir(steam_root().join("userdata")).ok()?;
    let mut seen_any = false;
    for entry in rd.flatten() {
        let cfg = entry.path().join("config/localconfig.vdf");
        if let Ok(text) = fs::read_to_string(cfg) {
            seen_any = true;
            if text.contains("twwh3-run") {
                return Some(true);
            }
        }
    }
    seen_any.then_some(false)
}

/// Whether something FUSE-y is mounted on `dir` (the game overlay, or
/// our own preview mount). /proc/mounts escapes spaces as \040.
pub(crate) fn overlay_mounted(dir: &Path) -> bool {
    let Ok(text) = fs::read_to_string("/proc/mounts") else {
        return false;
    };
    let target = dir.to_string_lossy().replace(' ', "\\040");
    text.lines().any(|l| {
        let mut f = l.split_whitespace();
        f.next(); // source
        f.next() == Some(&target) && f.next().is_some_and(|t| t.contains("fuse"))
    })
}

/// fuse-overlayfs from PATH, or the copy the Nix-wrapped twwh3-run
/// script carries on its own PATH line.
pub(crate) fn find_fuse_overlayfs() -> Option<PathBuf> {
    if let Some(p) = on_path("fuse-overlayfs") {
        return Some(p);
    }
    let script = fs::read_to_string(on_path("twwh3-run")?).ok()?;
    for piece in script.split(|c: char| c == '"' || c == ':' || c.is_whitespace()) {
        if piece.starts_with('/') && piece.contains("fuse-overlayfs") {
            let cand = Path::new(piece).join("fuse-overlayfs");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

pub(crate) fn mount_request_file() -> PathBuf {
    cache_dir().join("twwh3-mount-request")
}

pub(crate) fn unmount_request_file() -> PathBuf {
    cache_dir().join("twwh3-unmount-request")
}

/// Advertises that a TUI listener is live, so twwh3-run doesn't wait on a
/// request no one will service when the TUI is closed.
pub(crate) fn mount_listener_marker() -> PathBuf {
    cache_dir().join("twwh3-mount-listener")
}

/// twwh3-run records its definitive overlay decision here (epoch + text)
/// so the TUI can report which method a launch actually used.
pub(crate) fn overlay_status_file() -> PathBuf {
    cache_dir().join("twwh3-overlay-status")
}

/// Mount the staging overlay onto the game's data/ dir (host side). Only
/// ever targets the game's own data/, and clears any stale overlay first.
pub(crate) fn overlay_mount(data: &Path) -> bool {
    match game_install_dir() {
        Some(g) if g.join("data") == data => {}
        _ => return false,
    }
    if overlay_mounted(data) {
        overlay_unmount(data);
    }
    let Some(fo) = find_fuse_overlayfs() else { return false };
    Command::new(fo)
        .arg("-o")
        .arg(format!("lowerdir={}:{}", staging_dir().display(), data.display()))
        .arg(data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn overlay_unmount(data: &Path) -> bool {
    if !overlay_mounted(data) {
        return true;
    }
    Command::new("fusermount3")
        .args(["-u"])
        .arg(data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Spawn the background thread that services twwh3-run's mount/unmount
/// requests, and drop the marker advertising it. Requests are tiny files
/// holding the target data/ path.
pub(crate) fn start_mount_listener() {
    let _ = fs::create_dir_all(cache_dir());
    let _ = fs::remove_file(mount_request_file());
    let _ = fs::remove_file(unmount_request_file());
    let _ = fs::write(mount_listener_marker(), std::process::id().to_string());
    thread::spawn(|| loop {
        if let Ok(data) = fs::read_to_string(mount_request_file()) {
            let _ = fs::remove_file(mount_request_file());
            let data = data.trim();
            if !data.is_empty() {
                overlay_mount(Path::new(data));
            }
        }
        if let Ok(data) = fs::read_to_string(unmount_request_file()) {
            let _ = fs::remove_file(unmount_request_file());
            let data = data.trim();
            if !data.is_empty() {
                overlay_unmount(Path::new(data));
            }
        }
        thread::sleep(Duration::from_millis(150));
    });
}

/// Retract the listener marker and clear pending requests on shutdown.
/// Does not unmount: a game launched from this session may still be
/// running; a leaked overlay is cleared by the next mount instead.
pub(crate) fn stop_mount_listener() {
    let _ = fs::remove_file(mount_listener_marker());
    let _ = fs::remove_file(mount_request_file());
    let _ = fs::remove_file(unmount_request_file());
}

/// What `o` opens directories with: the open_with setting if given,
/// else xdg-open. If xdg-open sends folders to the wrong application on
/// your system, set open_with (or fix the inode/directory MIME handler).
pub(crate) fn dir_opener() -> Option<PathBuf> {
    if let Some(cmd) = setting("TWWH3_OPEN", "open_with") {
        let p = expand_path(&cmd);
        return if p.is_absolute() { Some(p) } else { on_path(&cmd) };
    }
    on_path("xdg-open")
}

/// Open a directory in the user's file manager, detached from the TUI.
pub(crate) fn open_dir(dir: &Path) -> bool {
    let Some(opener) = dir_opener() else { return false };
    Command::new(opener)
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Status-line hint when no directory opener could be found.
pub(crate) const OPENER_HINT: &str = "no file manager found — set open_with in ~/.config/twwh3-mods/config";

