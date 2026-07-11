//! Configuration & path resolution.
//!
//! Every path resolves in order: environment variable, config file,
//! default. The config file is ~/.config/twwh3-mods/config with
//! `key = value` lines, `#` comments, optional quotes, and `~/` expansion.

use crate::APPID;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(crate) fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub(crate) fn config_file() -> PathBuf {
    env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
        .join("twwh3-mods/config")
}

static CONFIG: OnceLock<HashMap<String, String>> = OnceLock::new();

fn config() -> &'static HashMap<String, String> {
    CONFIG.get_or_init(|| {
        let mut map = HashMap::new();
        if let Ok(text) = fs::read_to_string(config_file()) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    map.insert(k.trim().to_string(), v.to_string());
                }
            }
        }
        map
    })
}

/// Env var wins over the config file.
pub(crate) fn setting(env_key: &str, conf_key: &str) -> Option<String> {
    env::var(env_key)
        .ok()
        .or_else(|| config().get(conf_key).cloned())
}

pub(crate) fn expand_path(v: &str) -> PathBuf {
    match v.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(v),
    }
}

pub(crate) fn path_setting(env_key: &str, conf_key: &str) -> Option<PathBuf> {
    setting(env_key, conf_key).map(|v| expand_path(&v))
}

pub(crate) fn steam_root() -> PathBuf {
    path_setting("STEAM_ROOT", "steam_root").unwrap_or_else(|| home().join(".local/share/Steam"))
}

/// Base dir for this tool's own data (profiles, versioned mods, local mods).
pub(crate) fn data_dir() -> PathBuf {
    path_setting("TWWH3_DATA", "data_dir").unwrap_or_else(|| home().join("Games/TotalWarWH3"))
}

pub(crate) fn moddata_path() -> PathBuf {
    path_setting("TWWH3_MODDATA", "moddata").unwrap_or_else(|| {
        steam_root().join(format!(
            "steamapps/compatdata/{APPID}/pfx/drive_c/users/steamuser/\
             AppData/Roaming/The Creative Assembly/Launcher/20190104-moddata.dat"
        ))
    })
}

pub(crate) fn modlists_dir() -> PathBuf {
    path_setting("TWWH3_MODLISTS", "modlists").unwrap_or_else(|| data_dir().join("modlists"))
}

/// Remembers which profile is current across restarts.
pub(crate) fn current_profile_file() -> PathBuf {
    modlists_dir().join(".current")
}

/// Persisted custom ordering of profiles (one name per line).
pub(crate) fn profile_order_file() -> PathBuf {
    modlists_dir().join(".order")
}

pub(crate) fn workshop_dir() -> PathBuf {
    path_setting("TWWH3_WORKSHOP", "workshop")
        .unwrap_or_else(|| steam_root().join(format!("steamapps/workshop/content/{APPID}")))
}

/// Put a subfolder here per local (non-Workshop) mod:
/// local_mods/<name>/ holding its .pack file(s) and any loose assets,
/// mirrored into the game's data/.
pub(crate) fn local_mods_dir() -> PathBuf {
    if let Some(p) = path_setting("TWWH3_LOCAL", "local_mods") {
        return p;
    }
    let dir = data_dir().join("local_mods");
    // One-time migration from the old default name ("mods").
    let old = data_dir().join("mods");
    if !dir.exists() && old.is_dir() {
        let _ = fs::rename(&old, &dir);
    }
    dir
}

/// Local store of exact Steam Workshop mod versions, keyed by workshop id
/// and manifest: versioned_workshop_mods/<steam_id>/<manifest>/<files>.
pub(crate) fn versioned_mods_dir() -> PathBuf {
    if let Some(p) = path_setting("TWWH3_VERSIONED_WORKSHOP_MODS", "versioned_workshop_mods") {
        return p;
    }
    let dir = data_dir().join("versioned_workshop_mods");
    // One-time migration from the old default name ("vault").
    let old = data_dir().join("vault");
    if !dir.exists() && old.is_dir() {
        let _ = fs::rename(&old, &dir);
    }
    dir
}

/// Staging folder: the load order materialized as a mirror of symlinks,
/// each pointing at the exact file the game should read (versioned
/// workshop copy, pinned version, or local mod). Rebuilt on every launch.
pub(crate) fn staging_dir() -> PathBuf {
    path_setting("TWWH3_STAGING", "staging").unwrap_or_else(|| data_dir().join("staging"))
}

pub(crate) fn cache_dir() -> PathBuf {
    env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"))
}

/// Convert a unix path to the wine path the game sees under Proton.
pub(crate) fn unix_to_win(p: &Path) -> String {
    format!("Z:{}", p.display())
}

/// Shorten a path for display by replacing $HOME with ~.
pub(crate) fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    match env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => s.replacen(&h, "~", 1),
        _ => s,
    }
}
