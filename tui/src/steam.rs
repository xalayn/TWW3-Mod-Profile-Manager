//! Steam metadata: VDF/ACF parsing + the launcher's moddata (read-only).

use crate::paths::{moddata_path, path_setting, steam_root};
use crate::{APPID, GAME};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// First `"key" "value"` occurrence anywhere in a VDF document.
pub(crate) fn vdf_str(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let p: Vec<&str> = line.trim().split('"').collect();
        if p.len() >= 4 && p[1] == key {
            return Some(p[3].to_string());
        }
    }
    None
}

#[derive(Clone, Default)]
pub(crate) struct WsInfo {
    pub(crate) size: u64,
    pub(crate) timeupdated: u64,
    /// Steam depot manifest id — changes on every workshop update, so it
    /// identifies an exact version of a mod.
    pub(crate) manifest: String,
}

/// Per-item size/timeupdated/manifest from appworkshop_<appid>.acf.
pub(crate) fn parse_workshop_acf(text: &str) -> HashMap<String, WsInfo> {
    let mut map: HashMap<String, WsInfo> = HashMap::new();
    let mut pending: Option<String> = None;
    let mut cur: Option<(String, WsInfo)> = None;
    let mut depth = 0i32;
    let mut cur_depth = 0i32;
    for raw in text.lines() {
        let line = raw.trim();
        match line {
            "{" => {
                depth += 1;
                if cur.is_none() {
                    if let Some(id) = pending.take() {
                        cur = Some((id, WsInfo::default()));
                        cur_depth = depth;
                    }
                }
                pending = None;
            }
            "}" => {
                if depth == cur_depth {
                    if let Some((id, info)) = cur.take() {
                        // Each item appears twice: WorkshopItemsInstalled
                        // carries size (+ timeupdated, manifest);
                        // WorkshopItemDetails carries manifest (+
                        // timeupdated) but no size. Merge field-by-field so
                        // the second block can't wipe out the first's size.
                        let slot = map.entry(id).or_default();
                        if info.size != 0 {
                            slot.size = info.size;
                        }
                        if info.timeupdated != 0 {
                            slot.timeupdated = info.timeupdated;
                        }
                        if !info.manifest.is_empty() {
                            slot.manifest = info.manifest;
                        }
                    }
                }
                depth -= 1;
            }
            _ => {
                let p: Vec<&str> = line.split('"').collect();
                if p.len() >= 4 {
                    if let Some((_, info)) = cur.as_mut() {
                        match p[1] {
                            "size" => info.size = p[3].parse().unwrap_or(0),
                            "timeupdated" => info.timeupdated = p[3].parse().unwrap_or(0),
                            "manifest" => info.manifest = p[3].to_string(),
                            _ => {}
                        }
                    }
                } else if p.len() >= 2
                    && p[1].len() >= 6
                    && p[1].chars().all(|c| c.is_ascii_digit())
                {
                    pending = Some(p[1].to_string());
                } else {
                    pending = None;
                }
            }
        }
    }
    map
}

pub(crate) fn load_workshop_info() -> HashMap<String, WsInfo> {
    let path = steam_root().join(format!("steamapps/workshop/appworkshop_{APPID}.acf"));
    fs::read_to_string(path)
        .map(|t| parse_workshop_acf(&t))
        .unwrap_or_default()
}

pub(crate) fn load_game_buildid() -> Option<String> {
    let path = steam_root().join(format!("steamapps/appmanifest_{APPID}.acf"));
    vdf_str(&fs::read_to_string(path).ok()?, "buildid")
}

pub(crate) fn game_install_dir() -> Option<PathBuf> {
    if let Some(dir) = path_setting("TWWH3_GAME", "game_dir") {
        return dir.is_dir().then_some(dir);
    }
    let path = steam_root().join(format!("steamapps/appmanifest_{APPID}.acf"));
    let installdir = vdf_str(&fs::read_to_string(path).ok()?, "installdir")?;
    let dir = steam_root().join("steamapps/common").join(installdir);
    dir.is_dir().then_some(dir)
}

/// Per-pack metadata the CA launcher recorded, keyed by lowercased pack
/// file name. Read-only — used for Workshop names/descriptions and to
/// seed the initial load order, never written back.
pub(crate) struct ModMeta {
    pub(crate) name: String,
    pub(crate) category: String,
    pub(crate) short: String,
    pub(crate) active: bool,
    pub(crate) order: i64,
}

pub(crate) fn read_moddata_meta() -> HashMap<String, ModMeta> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(moddata_path()) else { return map };
    let Ok(root) = serde_json::from_str::<Value>(&text) else { return map };
    let Some(arr) = root.as_array() else { return map };
    for m in arr {
        if m.get("game").and_then(Value::as_str) != Some(GAME) {
            continue;
        }
        let Some(uuid) = m.get("uuid").and_then(Value::as_str) else { continue };
        map.insert(
            uuid.to_lowercase(),
            ModMeta {
                name: m.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                category: m.get("category").and_then(Value::as_str).unwrap_or("").to_string(),
                short: m.get("short").and_then(Value::as_str).unwrap_or("").to_string(),
                active: m.get("active").and_then(Value::as_bool).unwrap_or(false),
                order: m.get("order").and_then(Value::as_i64).unwrap_or(i64::MAX),
            },
        );
    }
    map
}

