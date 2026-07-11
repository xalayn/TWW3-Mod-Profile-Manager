//! The mod model: a mod is a folder. Discovery + load-order data types.

use crate::paths::{local_mods_dir, workshop_dir};
use crate::steam::ModMeta;
use ratatui_image::protocol::StatefulProtocol;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) enum Thumb {
    NotLoaded,
    Missing,
    Ready(StatefulProtocol),
}

/// A mod is a folder mirrored into the game's data/: a Steam Workshop
/// item dir (workshop/content/<appid>/<steam_id>/) or a local mod dir
/// (local_mods/<name>/). Either may hold several .pack files (+ loose assets
/// for local mods).
pub(crate) struct ModEntry {
    /// The mod's folder on disk.
    pub(crate) dir: PathBuf,
    /// Stable identity: steam_id for Workshop, folder name for local.
    pub(crate) id: String,
    /// Display name (Workshop title from moddata, else folder name).
    pub(crate) name: String,
    /// .pack files in the folder, sorted — the used_mods lines.
    pub(crate) packs: Vec<PathBuf>,
    /// Combined size of the packs, if known.
    pub(crate) size: Option<u64>,
    /// Workshop item id (Some = Workshop mod, None = local).
    pub(crate) steam_id: Option<String>,
    /// Preview image for the thumbnail, if any.
    pub(crate) png: Option<PathBuf>,
    pub(crate) thumb: Thumb,
    /// From the local mods dir rather than the Steam Workshop.
    pub(crate) local: bool,
    /// Workshop category (from moddata), for display only.
    pub(crate) category: String,
    /// Workshop description snippet (from moddata), for display only.
    pub(crate) short: String,
    /// The folder is gone / has no packs.
    pub(crate) missing: bool,
}

/// Every file under `root`, as (path relative to root, absolute path).
pub(crate) fn walk_files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    pub(crate) fn rec(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, PathBuf)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rec(base, &p, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push((rel.to_path_buf(), p.clone()));
            }
        }
    }
    let mut out = Vec::new();
    rec(root, root, &mut out);
    out
}

/// The .pack files directly discoverable under `dir` (any depth), sorted.
pub(crate) fn packs_in(dir: &Path) -> Vec<PathBuf> {
    let mut packs: Vec<PathBuf> = walk_files(dir)
        .into_iter()
        .map(|(_, abs)| abs)
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")))
        .collect();
    packs.sort();
    packs
}

/// A preview image in `dir` (any file with a .png extension), if present.
pub(crate) fn find_png(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
}

impl ModEntry {
    /// Build a mod from its folder. `steam_id` Some marks it a Workshop
    /// mod. Returns None if the folder has no .pack files.
    pub(crate) fn from_dir(dir: PathBuf, steam_id: Option<String>, name: String, local: bool) -> Option<Self> {
        let packs = packs_in(&dir);
        if packs.is_empty() {
            return None;
        }
        let size = packs
            .iter()
            .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
            .sum::<u64>()
            .into();
        let png = find_png(&dir);
        let id = steam_id.clone().unwrap_or_else(|| {
            dir.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string()
        });
        Some(ModEntry {
            dir,
            id,
            name,
            packs,
            size,
            steam_id,
            png,
            thumb: Thumb::NotLoaded,
            local,
            category: String::new(),
            short: String::new(),
            missing: false,
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn category(&self) -> &str {
        if self.category.is_empty() {
            "-"
        } else {
            &self.category
        }
    }

    /// Description snippet with BBCode-style [tags] stripped.
    pub(crate) fn description(&self) -> String {
        let mut out = String::with_capacity(self.short.len());
        let mut in_tag = false;
        for c in self.short.chars() {
            match c {
                '[' => in_tag = true,
                ']' => in_tag = false,
                '\r' => {}
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out
    }
}

/// Build the mod pool: every Workshop item dir and every local mod dir
/// that contains at least one .pack. `meta` supplies Workshop names from
/// the launcher's moddata.
pub(crate) fn discover_mods(meta: &HashMap<String, ModMeta>) -> Vec<ModEntry> {
    let mut pool: Vec<ModEntry> = Vec::new();

    // Workshop items: workshop/content/<appid>/<steam_id>/
    for item in fs::read_dir(workshop_dir()).into_iter().flatten().flatten() {
        let dir = item.path();
        if !dir.is_dir() {
            continue;
        }
        let steam_id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
            .map(String::from);
        let packs = packs_in(&dir);
        let Some(first) = packs.first() else { continue };
        // Name/metadata from moddata, matched by any of the item's packs.
        let m = packs.iter().find_map(|p| {
            let key = p.file_name()?.to_str()?.to_lowercase();
            meta.get(&key)
        });
        let name = m
            .map(|m| m.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                first.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string()
            });
        if let Some(mut e) = ModEntry::from_dir(dir, steam_id, name, false) {
            if let Some(m) = m {
                e.category = m.category.clone();
                e.short = m.short.clone();
            }
            pool.push(e);
        }
    }

    // Local mods: each subfolder of the local mods dir with packs.
    let ldir = local_mods_dir();
    let _ = fs::create_dir_all(&ldir);
    for item in fs::read_dir(&ldir).into_iter().flatten().flatten() {
        let p = item.path();
        if !p.is_dir() {
            continue;
        }
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        if let Some(e) = ModEntry::from_dir(p, None, name, true) {
            pool.push(e);
        }
    }

    pool.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    pool
}

/// One selectable version of a Workshop mod in the `v` picker.
pub(crate) struct VersionInfo {
    pub(crate) manifest: String,
    pub(crate) timeupdated: u64,
    pub(crate) size: u64,
    /// A faithful copy is in versioned_workshop_mods (can pin to it).
    pub(crate) stored: bool,
    /// Matches the version Steam has installed right now.
    pub(crate) current: bool,
}

/// One position in the load order: a mod id, resolved to the pool where
/// possible. `idx: None` means the mod is not installed at all.
pub(crate) struct Slot {
    pub(crate) id: String,
    pub(crate) idx: Option<usize>,
    /// Disabled slots keep their spot but are skipped at launch.
    pub(crate) enabled: bool,
    /// Ids of other mods this one requires (advisory dependency).
    pub(crate) requires: Vec<String>,
}

pub(crate) struct PreviewLine {
    pub(crate) label: String,
    pub(crate) missing: bool,
    pub(crate) updated: bool,
}

/// A profile entry as stored on disk. `id` is the mod key (Workshop
/// steam_id or local folder name); Workshop entries also pin a version.
pub(crate) struct MlEntry {
    pub(crate) id: String,
    pub(crate) local: bool,
    pub(crate) enabled: bool,
    pub(crate) requires: Vec<String>,
    pub(crate) steam_id: Option<String>,
    pub(crate) manifest: Option<String>,
    pub(crate) sha256: Option<String>,
}

/// The mod key of a stored profile entry, tolerating older profiles:
/// explicit `id`, else `steam_id` (Workshop), else legacy `uuid`.
pub(crate) fn ml_entry_id(e: &Value) -> Option<String> {
    e.get("id")
        .and_then(Value::as_str)
        .or_else(|| e.get("steam_id").and_then(Value::as_str))
        .or_else(|| e.get("uuid").and_then(Value::as_str))
        .map(String::from)
}

/// Where a resolved mod's files load from at launch.
#[derive(PartialEq, Clone, Copy)]
pub(crate) enum PackSource {
    /// Current Workshop version, served from the vault (vaulted on demand).
    Vault,
    /// Pinned Workshop version from the vault (Steam updated past the pin).
    Pinned,
    /// A local folder mod, loaded from its folder.
    Local,
    /// Workshop mod with no known manifest — loaded live from the workshop.
    Workshop,
}

impl PackSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            PackSource::Vault => "versioned",
            PackSource::Pinned => "versioned (pinned)",
            PackSource::Local => "local",
            PackSource::Workshop => "workshop (live)",
        }
    }
}

/// One mod of the resolved load order: its files to mirror into data/
/// (relative path, source) and the pack basenames for used_mods.
pub(crate) struct ResolvedMod {
    pub(crate) name: String,
    pub(crate) files: Vec<(PathBuf, PathBuf)>,
    pub(crate) packs: Vec<String>,
    pub(crate) source: PackSource,
    /// Pool index, so launch can vault the mod's current version.
    pub(crate) idx: usize,
    /// Chosen Workshop manifest (None for local / unknown).
    pub(crate) manifest: Option<String>,
}

/// Keep the last occurrence of each key, preserving order — load-order
/// override: a later mod's file/pack wins over an earlier one's.
pub(crate) fn dedup_last<T, K: Eq + std::hash::Hash>(v: Vec<T>, key: impl Fn(&T) -> K) -> Vec<T> {
    let mut seen = HashSet::new();
    let mut out: Vec<T> = Vec::new();
    for item in v.into_iter().rev() {
        if seen.insert(key(&item)) {
            out.push(item);
        }
    }
    out.reverse();
    out
}

