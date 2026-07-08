//! twwh3-mods — TUI load-order manager for Total War: WARHAMMER III.
//!
//! A mod is a *folder* that gets mirrored into the game's data/ at launch:
//! a Steam Workshop item dir (workshop/content/<appid>/<steam_id>/) or a
//! local mod dir (local_mods/<name>/). Either may hold several .pack files plus
//! loose assets (movies, tables); the packs are listed in used_mods, the
//! whole folder rides along via the overlay. Local mods are first-class —
//! you don't need Steam Workshop at all.
//!
//! Two panes: "Available" lists every mod on disk (Workshop scan + local
//! folders); "Load order" is the ordered list that will be enabled. The
//! load order lives in profiles (TWWH3_MODLISTS, default
//! ~/Games/TotalWarWH3/modlists); the launcher's moddata.dat is read for
//! Workshop names and to seed a first-run load order, but is never
//! written — launching goes through twwh3-run's staging/overlay.
//!
//! Profile entries whose mod isn't installed are shown as missing, skipped
//! at launch, and preserved in the profile file.

use anyhow::{bail, Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
    Wrap,
};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APPID: u32 = 1142710;
const GAME: &str = "warhammer3";

// ---------------------------------------------------------------------------
// Configuration & paths
//
// Every path resolves in order: environment variable, config file,
// default. The config file is ~/.config/twwh3-mods/config with
// `key = value` lines, `#` comments, optional quotes, and `~/` expansion.

fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn config_file() -> PathBuf {
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
fn setting(env_key: &str, conf_key: &str) -> Option<String> {
    env::var(env_key)
        .ok()
        .or_else(|| config().get(conf_key).cloned())
}

fn expand_path(v: &str) -> PathBuf {
    match v.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(v),
    }
}

fn path_setting(env_key: &str, conf_key: &str) -> Option<PathBuf> {
    setting(env_key, conf_key).map(|v| expand_path(&v))
}

fn steam_root() -> PathBuf {
    path_setting("STEAM_ROOT", "steam_root")
        .unwrap_or_else(|| home().join(".local/share/Steam"))
}

/// Base dir for this tool's own data (profiles, versioned mods, local mods).
fn data_dir() -> PathBuf {
    path_setting("TWWH3_DATA", "data_dir").unwrap_or_else(|| home().join("Games/TotalWarWH3"))
}

fn moddata_path() -> PathBuf {
    path_setting("TWWH3_MODDATA", "moddata").unwrap_or_else(|| {
        steam_root().join(format!(
            "steamapps/compatdata/{APPID}/pfx/drive_c/users/steamuser/\
             AppData/Roaming/The Creative Assembly/Launcher/20190104-moddata.dat"
        ))
    })
}

fn modlists_dir() -> PathBuf {
    path_setting("TWWH3_MODLISTS", "modlists").unwrap_or_else(|| data_dir().join("modlists"))
}

/// Remembers which profile is current across restarts.
fn current_profile_file() -> PathBuf {
    modlists_dir().join(".current")
}

fn workshop_dir() -> PathBuf {
    path_setting("TWWH3_WORKSHOP", "workshop")
        .unwrap_or_else(|| steam_root().join(format!("steamapps/workshop/content/{APPID}")))
}

/// Put a subfolder here per local (non-Workshop) mod:
/// local_mods/<name>/ holding its .pack file(s) and any loose assets,
/// mirrored into the game's data/.
fn local_mods_dir() -> PathBuf {
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
fn versioned_mods_dir() -> PathBuf {
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

/// Staging folder: the load order materialized as one symlink per pack,
/// each pointing at the exact file the game should read (workshop copy,
/// vaulted pin, or local mod). Rebuilt on every launch.
fn staging_dir() -> PathBuf {
    path_setting("TWWH3_STAGING", "staging").unwrap_or_else(|| data_dir().join("staging"))
}

/// Convert a unix path to the wine path the game sees under Proton.
fn unix_to_win(p: &Path) -> String {
    format!("Z:{}", p.display())
}

// ---------------------------------------------------------------------------
// Steam manifests (VDF text format, parsed just enough for our needs)

/// First `"key" "value"` occurrence anywhere in a VDF document.
fn vdf_str(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let p: Vec<&str> = line.trim().split('"').collect();
        if p.len() >= 4 && p[1] == key {
            return Some(p[3].to_string());
        }
    }
    None
}

#[derive(Clone, Default)]
struct WsInfo {
    size: u64,
    timeupdated: u64,
    /// Steam depot manifest id — changes on every workshop update, so it
    /// identifies an exact version of a mod.
    manifest: String,
}

/// Per-item size/timeupdated/manifest from appworkshop_<appid>.acf.
fn parse_workshop_acf(text: &str) -> HashMap<String, WsInfo> {
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

fn load_workshop_info() -> HashMap<String, WsInfo> {
    let path = steam_root().join(format!("steamapps/workshop/appworkshop_{APPID}.acf"));
    fs::read_to_string(path)
        .map(|t| parse_workshop_acf(&t))
        .unwrap_or_default()
}

fn load_game_buildid() -> Option<String> {
    let path = steam_root().join(format!("steamapps/appmanifest_{APPID}.acf"));
    vdf_str(&fs::read_to_string(path).ok()?, "buildid")
}

fn game_install_dir() -> Option<PathBuf> {
    if let Some(dir) = path_setting("TWWH3_GAME", "game_dir") {
        return dir.is_dir().then_some(dir);
    }
    let path = steam_root().join(format!("steamapps/appmanifest_{APPID}.acf"));
    let installdir = vdf_str(&fs::read_to_string(path).ok()?, "installdir")?;
    let dir = steam_root().join("steamapps/common").join(installdir);
    dir.is_dir().then_some(dir)
}

// ---------------------------------------------------------------------------
// Mod entries

enum Thumb {
    NotLoaded,
    Missing,
    Ready(StatefulProtocol),
}

/// A mod is a folder mirrored into the game's data/: a Steam Workshop
/// item dir (workshop/content/<appid>/<steam_id>/) or a local mod dir
/// (local_mods/<name>/). Either may hold several .pack files (+ loose assets
/// for local mods).
struct ModEntry {
    /// The mod's folder on disk.
    dir: PathBuf,
    /// Stable identity: steam_id for Workshop, folder name for local.
    id: String,
    /// Display name (Workshop title from moddata, else folder name).
    name: String,
    /// .pack files in the folder, sorted — the used_mods lines.
    packs: Vec<PathBuf>,
    /// Combined size of the packs, if known.
    size: Option<u64>,
    /// Workshop item id (Some = Workshop mod, None = local).
    steam_id: Option<String>,
    /// Preview image for the thumbnail, if any.
    png: Option<PathBuf>,
    thumb: Thumb,
    /// From the local mods dir rather than the Steam Workshop.
    local: bool,
    /// Workshop category (from moddata), for display only.
    category: String,
    /// Workshop description snippet (from moddata), for display only.
    short: String,
    /// The folder is gone / has no packs.
    missing: bool,
}

/// Every file under `root`, as (path relative to root, absolute path).
fn walk_files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    fn rec(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, PathBuf)>) {
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
fn packs_in(dir: &Path) -> Vec<PathBuf> {
    let mut packs: Vec<PathBuf> = walk_files(dir)
        .into_iter()
        .map(|(_, abs)| abs)
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")))
        .collect();
    packs.sort();
    packs
}

/// A preview image in `dir` (any file with a .png extension), if present.
fn find_png(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
}

impl ModEntry {
    /// Build a mod from its folder. `steam_id` Some marks it a Workshop
    /// mod. Returns None if the folder has no .pack files.
    fn from_dir(dir: PathBuf, steam_id: Option<String>, name: String, local: bool) -> Option<Self> {
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

    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> &str {
        if self.category.is_empty() {
            "-"
        } else {
            &self.category
        }
    }

    /// Description snippet with BBCode-style [tags] stripped.
    fn description(&self) -> String {
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

/// Per-pack metadata the CA launcher recorded, keyed by lowercased pack
/// file name. Read-only — used for Workshop names/descriptions and to
/// seed the initial load order, never written back.
struct ModMeta {
    name: String,
    category: String,
    short: String,
    active: bool,
    order: i64,
}

fn read_moddata_meta() -> HashMap<String, ModMeta> {
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

/// Build the mod pool: every Workshop item dir and every local mod dir
/// that contains at least one .pack. `meta` supplies Workshop names from
/// the launcher's moddata.
fn discover_mods(meta: &HashMap<String, ModMeta>) -> Vec<ModEntry> {
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

/// Copy a Workshop mod's whole folder (packs, preview, any loose files)
/// into vault/<sid>/<manifest>/, skipping files already present, so the
/// vaulted version is a faithful copy that survives unsubscribe. Records
/// a sha256 sidecar per pack. Returns how many packs were newly vaulted.
fn store_mod_version(sid: &str, manifest: &str, dir: &Path) -> Result<usize> {
    if manifest.is_empty() {
        return Ok(0);
    }
    let dst_dir = versioned_mods_dir().join(sid).join(manifest);
    let mut n = 0usize;
    for (rel, src) in walk_files(dir) {
        let dst = dst_dir.join(&rel);
        if dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let fname = rel.file_name().and_then(|s| s.to_str()).unwrap_or("f");
        // Copy to a temp name first so an interrupted copy can't be
        // mistaken for a complete vaulted file.
        let tmp = dst.with_file_name(format!(".{fname}.tmp"));
        fs::copy(&src, &tmp).with_context(|| format!("could not vault {}", src.display()))?;
        fs::rename(&tmp, &dst)?;
        if src.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")) {
            // A content hash beside the pack lets a version be verified
            // independently of Steam (used when importing a bundle).
            if let Some(hash) = pack_sha256(&dst) {
                let _ = fs::write(sha256_sidecar(&dst), hash);
            }
            n += 1;
        }
    }
    Ok(n)
}

/// Streaming SHA-256 of a file, hex-encoded. `manifest` (Steam's depot
/// GID) names an exact *version*; this hashes the actual pack bytes.
fn pack_sha256(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    let mut hex = String::with_capacity(64);
    use std::fmt::Write as _;
    for b in hasher.finalize() {
        let _ = write!(hex, "{b:02x}");
    }
    Some(hex)
}

/// The `<pack>.sha256` sidecar path next to a vaulted pack.
fn sha256_sidecar(pack: &Path) -> PathBuf {
    let mut name = pack.file_name().unwrap_or_default().to_os_string();
    name.push(".sha256");
    pack.with_file_name(name)
}

/// The `.pack` file inside a vault version directory, if any.
fn find_stored_pack(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")))
}

/// A profile name in `dir` that doesn't collide, appending " 2", " 3", …
/// to `base` if needed. Non-destructive: never overwrites an existing
/// profile.
fn unique_profile_name(dir: &Path, base: &str) -> String {
    if !dir.join(format!("{base}.json")).exists() {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|c| !dir.join(format!("{c}.json")).exists())
        .unwrap_or_else(|| base.to_string())
}

/// Append in-memory bytes to a tar under `name`.
fn tar_append_bytes<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, name, data)?;
    Ok(())
}

/// The content hash of a vaulted pack, from its sidecar (or computed and
/// cached if the sidecar is absent — e.g. an older vault).
fn read_or_make_sha256(pack: &Path) -> Option<String> {
    let sidecar = sha256_sidecar(pack);
    if let Ok(s) = fs::read_to_string(&sidecar) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    let hash = pack_sha256(pack)?;
    let _ = fs::write(&sidecar, &hash);
    Some(hash)
}

/// Rebuild the staging folder to mirror `entries` — each (relative path,
/// source) becomes a symlink at staging/<rel> pointing at the source.
/// Parent dirs are created; later entries override earlier ones (load-
/// order precedence). The folder is rebuilt from scratch each launch.
fn rebuild_staging(staging: &Path, entries: &[(PathBuf, PathBuf)]) -> Result<()> {
    if staging.exists() {
        fs::remove_dir_all(staging)
            .with_context(|| format!("could not clear staging dir {}", staging.display()))?;
    }
    fs::create_dir_all(staging)
        .with_context(|| format!("could not create staging dir {}", staging.display()))?;
    for (rel, src) in entries {
        let dst = staging.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if dst.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&dst);
        }
        symlink(src, &dst)
            .with_context(|| format!("could not link {} into staging", rel.display()))?;
    }
    Ok(())
}

/// Where a specific vaulted pack version lives, if it exists.
fn stored_pack_path(sid: &str, manifest: &str, pack: &Path) -> Option<PathBuf> {
    let fname = pack.file_name()?;
    let p = versioned_mods_dir().join(sid).join(manifest).join(fname);
    p.exists().then_some(p)
}

fn human_size(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    match b {
        _ if b >= GIB => format!("{:.1} GiB", b as f64 / GIB as f64),
        _ if b >= MIB => format!("{:.1} MiB", b as f64 / MIB as f64),
        _ if b >= KIB => format!("{:.0} KiB", b as f64 / KIB as f64),
        _ => format!("{b} B"),
    }
}

// ---------------------------------------------------------------------------
// Status page checks

/// Where `bin` resolves on this shell's PATH, if anywhere. (Steam may
/// see a different PATH, so this is a best-effort signal.)
fn on_path(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Whether any Steam user's launch options mention twwh3-run. Steam
/// keeps launch options in userdata/<id>/config/localconfig.vdf.
/// None = could not read any config (unknown).
fn launch_options_use_shim() -> Option<bool> {
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

/// (vaulted versions, total bytes) across the whole vault.
fn versioned_mods_stats() -> (usize, u64) {
    let mut versions = 0usize;
    let mut bytes = 0u64;
    for id_dir in fs::read_dir(versioned_mods_dir()).into_iter().flatten().flatten() {
        for man_dir in fs::read_dir(id_dir.path()).into_iter().flatten().flatten() {
            if !man_dir.path().is_dir() {
                continue;
            }
            versions += 1;
            for f in fs::read_dir(man_dir.path()).into_iter().flatten().flatten() {
                bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (versions, bytes)
}

/// Whether something FUSE-y is mounted on `dir` (the game overlay, or
/// our own preview mount). /proc/mounts escapes spaces as \040.
fn overlay_mounted(dir: &Path) -> bool {
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
fn find_fuse_overlayfs() -> Option<PathBuf> {
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

// ---------------------------------------------------------------------------
// Out-of-sandbox mount listener
//
// On a bwrapped Steam (e.g. NixOS's FHS build) the game — and the
// twwh3-run shim launched with it — run inside a bubblewrap sandbox with
// PR_SET_NO_NEW_PRIVS, which neuters setuid fusermount3, so the overlay
// can't be mounted from in there. But a mount created on the host after
// the sandbox starts propagates into it (bwrap mounts are rslave of the
// host). So the TUI, which runs outside the sandbox, does the mount on
// twwh3-run's behalf: twwh3-run drops a request file in the shared cache
// dir, this listener performs the mount, and the shim sees data/ become a
// mountpoint from inside. The cache dir is known to be shared into the
// sandbox — twwh3-run already writes its log there.

fn cache_dir() -> PathBuf {
    env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"))
}

fn mount_request_file() -> PathBuf {
    cache_dir().join("twwh3-mount-request")
}

fn unmount_request_file() -> PathBuf {
    cache_dir().join("twwh3-unmount-request")
}

/// Advertises that a TUI listener is live, so twwh3-run doesn't wait on a
/// request no one will service when the TUI is closed.
fn mount_listener_marker() -> PathBuf {
    cache_dir().join("twwh3-mount-listener")
}

/// twwh3-run records its definitive overlay decision here (epoch + text)
/// so the TUI can report which method a launch actually used.
fn overlay_status_file() -> PathBuf {
    cache_dir().join("twwh3-overlay-status")
}

/// Mount the staging overlay onto the game's data/ dir (host side). Only
/// ever targets the game's own data/, and clears any stale overlay first.
fn overlay_mount(data: &Path) -> bool {
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

fn overlay_unmount(data: &Path) -> bool {
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
fn start_mount_listener() {
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
fn stop_mount_listener() {
    let _ = fs::remove_file(mount_listener_marker());
    let _ = fs::remove_file(mount_request_file());
    let _ = fs::remove_file(unmount_request_file());
}

/// What `o` opens directories with: the open_with setting if given,
/// else xdg-open. If xdg-open sends folders to the wrong application on
/// your system, set open_with (or fix the inode/directory MIME handler).
fn dir_opener() -> Option<PathBuf> {
    if let Some(cmd) = setting("TWWH3_OPEN", "open_with") {
        let p = expand_path(&cmd);
        return if p.is_absolute() { Some(p) } else { on_path(&cmd) };
    }
    on_path("xdg-open")
}

/// Open a directory in the user's file manager, detached from the TUI.
fn open_dir(dir: &Path) -> bool {
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
const OPENER_HINT: &str = "no file manager found — set open_with in ~/.config/twwh3-mods/config";

/// Shorten a path for display by replacing $HOME with ~.
fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    match env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => s.replacen(&h, "~", 1),
        _ => s,
    }
}

/// One row of the status page. `ok` drives the colour: true = green,
/// false = red, None = informational. An empty value marks a section
/// header.
struct StatusLine {
    label: String,
    value: String,
    ok: Option<bool>,
}

impl StatusLine {
    fn section(label: &str) -> Self {
        Self { label: label.into(), value: String::new(), ok: None }
    }
    fn new(label: &str, value: impl Into<String>, ok: Option<bool>) -> Self {
        Self { label: label.into(), value: value.into(), ok }
    }
}

/// A path row: green when it exists, `missing_note` (with `missing_ok`
/// severity) when it doesn't.
fn path_line(label: &str, p: &Path, missing_note: &str, missing_ok: Option<bool>) -> StatusLine {
    if p.exists() {
        StatusLine::new(label, tilde(p), Some(true))
    } else {
        StatusLine::new(label, format!("{} {}", tilde(p), missing_note), missing_ok)
    }
}

// ---------------------------------------------------------------------------
// App state

#[derive(PartialEq)]
enum Mode {
    Browse,
    Profiles,
    NameInput,
    Status,
    Help,
}

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Available,
    Profile,
}

/// One position in the load order: a mod id, resolved to the pool where
/// possible. `idx: None` means the mod is not installed at all.
struct Slot {
    id: String,
    idx: Option<usize>,
}

struct PreviewLine {
    label: String,
    missing: bool,
    updated: bool,
}

/// A profile entry as stored on disk. `id` is the mod key (Workshop
/// steam_id or local folder name); Workshop entries also pin a version.
struct MlEntry {
    id: String,
    local: bool,
    steam_id: Option<String>,
    manifest: Option<String>,
    sha256: Option<String>,
}

/// The mod key of a stored profile entry, tolerating older profiles:
/// explicit `id`, else `steam_id` (Workshop), else legacy `uuid`.
fn ml_entry_id(e: &Value) -> Option<String> {
    e.get("id")
        .and_then(Value::as_str)
        .or_else(|| e.get("steam_id").and_then(Value::as_str))
        .or_else(|| e.get("uuid").and_then(Value::as_str))
        .map(String::from)
}

/// Where a resolved mod's files load from at launch.
#[derive(PartialEq, Clone, Copy)]
enum PackSource {
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
    fn label(self) -> &'static str {
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
struct ResolvedMod {
    name: String,
    files: Vec<(PathBuf, PathBuf)>,
    packs: Vec<String>,
    source: PackSource,
    /// Pool index, so launch can vault the mod's current version.
    idx: usize,
    /// Chosen Workshop manifest (None for local / unknown).
    manifest: Option<String>,
}

/// Keep the last occurrence of each key, preserving order — load-order
/// override: a later mod's file/pack wins over an earlier one's.
fn dedup_last<T, K: Eq + std::hash::Hash>(v: Vec<T>, key: impl Fn(&T) -> K) -> Vec<T> {
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

struct App {
    /// The launcher moddata file, read for Workshop names/seed (not written).
    path: PathBuf,
    /// Every known mod (Workshop + local folders). Never reordered.
    pool: Vec<ModEntry>,
    /// The load order: mods in here are enabled, in this order.
    slots: Vec<Slot>,
    focus: Pane,
    avail_state: TableState,
    prof_state: TableState,
    picker: Option<Picker>,
    dirty: bool,
    status: String,
    confirm_quit: bool,
    mode: Mode,
    /// Profile that was last applied or saved; `s` keeps it in sync.
    current_profile: Option<String>,
    /// Current workshop state: steam_id -> version info.
    ws: HashMap<String, WsInfo>,
    game_buildid: Option<String>,
    /// Version pins of the current profile: uuid -> pinned manifest.
    pins: HashMap<String, String>,
    pinned_buildid: Option<String>,
    profiles: Vec<String>,
    profile_list: ListState,
    /// Which profile the cached popup preview belongs to.
    preview_for: Option<String>,
    preview: Vec<PreviewLine>,
    confirm_delete: bool,
    input: String,
    /// When set, the name input renames this profile instead of creating.
    rename_from: Option<String>,
    /// Rows of the status page, computed when it is opened (S).
    status_lines: Vec<StatusLine>,
    /// We mounted a data/ overlay preview (o) and must unmount it.
    preview_mounted: bool,
    /// After L, watch twwh3-run's overlay-status file for a report newer
    /// than this launch epoch, to show which method the game launched with.
    overlay_watch_since: Option<u64>,
}

impl App {
    fn load(path: PathBuf, picker: Option<Picker>) -> Result<Self> {
        // moddata is read only for Workshop names + to seed the initial
        // load order; it's optional (a Workshop-free setup has none). The
        // pool is built by scanning the Workshop and local mod folders.
        let meta = read_moddata_meta();
        let pool = discover_mods(&meta);

        let mut app = App {
            path,
            pool,
            slots: Vec::new(),
            focus: Pane::Profile,
            avail_state: TableState::default(),
            prof_state: TableState::default(),
            picker,
            dirty: false,
            status: String::new(),
            confirm_quit: false,
            mode: Mode::Browse,
            current_profile: None,
            ws: load_workshop_info(),
            game_buildid: load_game_buildid(),
            pins: HashMap::new(),
            pinned_buildid: None,
            profiles: Vec::new(),
            profile_list: ListState::default(),
            preview_for: None,
            preview: Vec::new(),
            confirm_delete: false,
            input: String::new(),
            rename_from: None,
            status_lines: Vec::new(),
            preview_mounted: false,
            overlay_watch_since: None,
        };

        // Initial load order: the current profile if it still exists, else
        // seed from the launcher's active mods so existing setups carry.
        app.current_profile = fs::read_to_string(current_profile_file())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|n| !n.is_empty())
            .filter(|n| modlists_dir().join(format!("{n}.json")).exists());
        match app.current_profile.clone() {
            Some(name) => {
                app.slots = app.slots_for_profile(&name);
                app.load_pins(&name);
                app.status = app.drift_report().unwrap_or_default();
            }
            None => app.slots = app.slots_from_moddata(&meta),
        }

        if !app.slots.is_empty() {
            app.prof_state.select(Some(0));
        }
        if !app.available().is_empty() {
            app.avail_state.select(Some(0));
        }
        if app.slots.is_empty() {
            app.focus = Pane::Available;
        }
        Ok(app)
    }

    /// Resolve a profile's entries to load-order slots against the pool.
    fn slots_for_profile(&self, name: &str) -> Vec<Slot> {
        Self::read_modlist(name)
            .unwrap_or_default()
            .into_iter()
            .map(|e| {
                let idx = self.pool.iter().position(|m| m.id().eq_ignore_ascii_case(&e.id));
                Slot { id: e.id, idx }
            })
            .collect()
    }

    /// Seed the load order from the launcher's active mods (mapped to
    /// their folders) so users with no profile keep their current setup.
    fn slots_from_moddata(&self, meta: &HashMap<String, ModMeta>) -> Vec<Slot> {
        let mut active: Vec<(&String, &ModMeta)> = meta.iter().filter(|(_, m)| m.active).collect();
        active.sort_by_key(|(_, m)| m.order);
        let mut seen: HashSet<String> = HashSet::new();
        let mut slots = Vec::new();
        for (packname, _) in active {
            let Some(idx) = self.pool.iter().position(|e| {
                e.packs.iter().any(|p| {
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.eq_ignore_ascii_case(packname))
                })
            }) else {
                continue;
            };
            let id = self.pool[idx].id().to_string();
            if seen.insert(id.clone()) {
                slots.push(Slot { id, idx: Some(idx) });
            }
        }
        slots
    }

    /// Load the version pins recorded in a profile file.
    fn load_pins(&mut self, name: &str) {
        self.pins.clear();
        self.pinned_buildid = None;
        let path = modlists_dir().join(format!("{name}.json"));
        let Some(root) = fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return;
        };
        self.pinned_buildid = root
            .get("game_buildid")
            .and_then(Value::as_str)
            .map(String::from);
        for e in root.get("mods").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(id), Some(man)) =
                (ml_entry_id(e), e.get("manifest").and_then(Value::as_str))
            {
                self.pins.insert(id, man.to_string());
            }
        }
    }

    /// The pinned manifest differs from what's installed now.
    fn slot_updated(&self, s: &Slot) -> bool {
        let Some(pinned) = self.pins.get(&s.id) else { return false };
        let Some(i) = s.idx else { return false };
        let Some(sid) = &self.pool[i].steam_id else { return false };
        self.ws
            .get(sid)
            .is_some_and(|w| !w.manifest.is_empty() && &w.manifest != pinned)
    }

    /// Human summary of what changed since the current profile was saved.
    fn drift_report(&self) -> Option<String> {
        let name = self.current_profile.as_deref()?;
        let updated = self.slots.iter().filter(|s| self.slot_updated(s)).count();
        let game_moved = match (&self.pinned_buildid, &self.game_buildid) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };
        if updated == 0 && !game_moved {
            return None;
        }
        let mut parts = Vec::new();
        if updated > 0 {
            parts.push(format!("{updated} mods updated by Steam"));
        }
        if game_moved {
            parts.push(format!(
                "game updated (build {} → {})",
                self.pinned_buildid.as_deref().unwrap_or("?"),
                self.game_buildid.as_deref().unwrap_or("?")
            ));
        }
        Some(format!(
            "Since '{name}' was saved: {} — L launches with pinned versions where stored",
            parts.join("; ")
        ))
    }

    /// Gather everything for the status page (S): resolved paths, the
    /// launch/overlay plumbing, and the current mod/profile state.
    fn build_status(&self) -> Vec<StatusLine> {
        let mut v = Vec::new();

        v.push(StatusLine::section("Paths"));
        v.push(path_line("config file", &config_file(), "(not present — defaults in use)", None));
        v.push(path_line("launcher data", &self.path, "(missing — run the game once)", Some(false)));
        v.push(path_line("workshop", &workshop_dir(), "(missing — no Workshop mods downloaded?)", Some(false)));
        let game = game_install_dir();
        match &game {
            Some(d) => v.push(StatusLine::new("game", tilde(d), Some(true))),
            None => v.push(StatusLine::new("game", "not found — set game_dir in the config", Some(false))),
        }
        v.push(path_line("profiles", &modlists_dir(), "(created on first save)", None));
        v.push(path_line("versioned mods", &versioned_mods_dir(), "(created on first save)", None));
        v.push(path_line("local mods", &local_mods_dir(), "(one subfolder per mod)", None));
        v.push(path_line("staging", &staging_dir(), "(created on first launch)", None));
        if let Some(g) = &game {
            v.push(path_line("used_mods.txt", &g.join("used_mods.txt"), "(written when you press L)", None));
        }

        v.push(StatusLine::section("Launch"));
        match on_path("twwh3-run") {
            Some(p) => v.push(StatusLine::new("twwh3-run", tilde(&p), Some(true))),
            None => v.push(StatusLine::new("twwh3-run", "not found on PATH", Some(false))),
        }
        match launch_options_use_shim() {
            Some(true) => v.push(StatusLine::new("launch options", "twwh3-run is set up in Steam", Some(true))),
            Some(false) => v.push(StatusLine::new(
                "launch options",
                "twwh3-run not set — Steam opens the CA launcher (no pinning/overlay)",
                Some(false),
            )),
            None => v.push(StatusLine::new("launch options", "could not read Steam userdata", None)),
        }
        let overlay = setting("TWWH3_OVERLAY", "overlay").unwrap_or_else(|| "on".into());
        if overlay == "off" {
            v.push(StatusLine::new("overlay", "off — working-directory loading (movie packs won't load)", None));
        } else {
            v.push(StatusLine::new("overlay", "on", Some(true)));
            match on_path("fuse-overlayfs") {
                Some(p) => v.push(StatusLine::new("fuse-overlayfs", tilde(&p), Some(true))),
                None => {
                    // The Nix-wrapped twwh3-run carries its own copy.
                    let bundled = on_path("twwh3-run")
                        .and_then(|p| fs::read_to_string(p).ok())
                        .is_some_and(|t| t.contains("fuse-overlayfs"));
                    v.push(if bundled {
                        StatusLine::new("fuse-overlayfs", "bundled with twwh3-run", Some(true))
                    } else {
                        StatusLine::new(
                            "fuse-overlayfs",
                            "not found — mods still load, movie packs won't (install fuse-overlayfs)",
                            Some(false),
                        )
                    });
                }
            }
            match on_path("fusermount3") {
                Some(p) => v.push(StatusLine::new("fusermount3", tilde(&p), Some(true))),
                None => v.push(StatusLine::new("fusermount3", "not found (install fuse3)", Some(false))),
            }
            v.push(if Path::new("/dev/fuse").exists() {
                StatusLine::new("/dev/fuse", "present", Some(true))
            } else {
                StatusLine::new("/dev/fuse", "missing — kernel FUSE unavailable", Some(false))
            });
            if let Some(g) = &game {
                let merged = overlay_mounted(&g.join("data"));
                v.push(StatusLine::new(
                    "data view",
                    if self.preview_mounted {
                        "merged — preview mounted (o to unmount)"
                    } else if merged {
                        "merged — overlay mounted now"
                    } else {
                        "vanilla until launch (o to preview)"
                    },
                    None,
                ));
            }
        }
        match dir_opener() {
            Some(p) => v.push(StatusLine::new("o opens with", tilde(&p), Some(true))),
            None => v.push(StatusLine::new("o opens with", OPENER_HINT, Some(false))),
        }

        v.push(StatusLine::section("Mods"));
        v.push(StatusLine::new(
            "game build",
            self.game_buildid.clone().unwrap_or_else(|| "unknown".into()),
            None,
        ));
        v.push(StatusLine::new(
            "profile",
            self.current_profile.clone().unwrap_or_else(|| "(unsaved)".into()),
            None,
        ));
        let missing = self.slots.iter().filter(|s| self.slot_missing(s)).count();
        let updated = self.slots.iter().filter(|s| self.slot_updated(s)).count();
        let mut lo = format!("{} mods", self.slots.len());
        if missing > 0 {
            lo.push_str(&format!(", {missing} missing"));
        }
        if updated > 0 {
            lo.push_str(&format!(", {updated} updated past their pin (stored version used at launch)"));
        }
        v.push(StatusLine::new("load order", lo, Some(missing == 0)));
        if self.dirty {
            v.push(StatusLine::new("unsaved changes", "yes — press s to save", Some(false)));
        }
        let total = self.pool.len();
        let local = self.pool.iter().filter(|m| m.local).count();
        let workshop = total - local;
        let mut pool = format!("{total} mods");
        if workshop > 0 {
            pool.push_str(&format!(", {workshop} workshop"));
        }
        if local > 0 {
            pool.push_str(&format!(", {local} local"));
        }
        v.push(StatusLine::new("mod pool", pool, None));
        let (versions, bytes) = versioned_mods_stats();
        v.push(StatusLine::new(
            "versioned mods",
            format!("{versions} pack versions, {}", human_size(bytes)),
            None,
        ));
        let images = setting("TWWH3_IMAGES", "images").unwrap_or_else(|| "auto".into());
        let active = if self.picker.is_some() { "" } else { " (inactive)" };
        v.push(StatusLine::new("thumbnails", format!("{images}{active}"), None));
        v
    }

    /// Change the current profile and persist the choice.
    fn set_current(&mut self, name: Option<String>) {
        self.current_profile = name;
        match &self.current_profile {
            Some(n) => {
                let _ = fs::create_dir_all(modlists_dir());
                let _ = fs::write(current_profile_file(), n);
            }
            None => {
                let _ = fs::remove_file(current_profile_file());
            }
        }
    }

    /// Pool indices of installed mods not currently in the load order,
    /// sorted by name.
    fn available(&self) -> Vec<usize> {
        let used: HashSet<usize> = self.slots.iter().filter_map(|s| s.idx).collect();
        let mut v: Vec<usize> = (0..self.pool.len())
            .filter(|i| !self.pool[*i].missing && !used.contains(i))
            .collect();
        v.sort_by_key(|&i| self.pool[i].name().to_lowercase());
        v
    }

    fn slot_missing(&self, s: &Slot) -> bool {
        s.idx.is_none_or(|i| self.pool[i].missing)
    }

    // -- navigation ---------------------------------------------------------

    fn focused_len(&self) -> usize {
        match self.focus {
            Pane::Available => self.available().len(),
            Pane::Profile => self.slots.len(),
        }
    }

    fn focused_state(&mut self) -> &mut TableState {
        match self.focus {
            Pane::Available => &mut self.avail_state,
            Pane::Profile => &mut self.prof_state,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.focused_len();
        let state = self.focused_state();
        let Some(i) = state.selected().filter(|&i| i < len) else { return };
        let j = (i as isize + delta).clamp(0, len as isize - 1);
        state.select(Some(j as usize));
    }

    fn select_edge(&mut self, end: bool) {
        let len = self.focused_len();
        let state = self.focused_state();
        if len > 0 {
            state.select(Some(if end { len - 1 } else { 0 }));
        }
    }

    fn switch_pane(&mut self) {
        self.focus = match self.focus {
            Pane::Available => Pane::Profile,
            Pane::Profile => Pane::Available,
        };
        let len = self.focused_len();
        let state = self.focused_state();
        if state.selected().is_none_or(|i| i >= len) {
            state.select(if len == 0 { None } else { Some(0) });
        }
    }

    // -- load-order edits ---------------------------------------------------

    /// Move the selected available mod into the load order.
    fn add_selected(&mut self) {
        let avail = self.available();
        let Some(sel) = self.avail_state.selected().filter(|&s| s < avail.len()) else {
            return;
        };
        let i = avail[sel];
        let id = self.pool[i].id().to_string();
        self.slots.push(Slot { id, idx: Some(i) });
        self.dirty = true;
        let left = avail.len() - 1;
        self.avail_state
            .select(if left == 0 { None } else { Some(sel.min(left - 1)) });
        if self.prof_state.selected().is_none() {
            self.prof_state.select(Some(self.slots.len() - 1));
        }
    }

    /// Remove the selected slot from the load order.
    fn remove_selected(&mut self) {
        let Some(sel) = self.prof_state.selected().filter(|&s| s < self.slots.len()) else {
            return;
        };
        self.slots.remove(sel);
        self.dirty = true;
        let n = self.slots.len();
        self.prof_state
            .select(if n == 0 { None } else { Some(sel.min(n - 1)) });
        if self.avail_state.selected().is_none() && !self.available().is_empty() {
            self.avail_state.select(Some(0));
        }
    }

    fn move_slot(&mut self, delta: isize) {
        if self.focus != Pane::Profile {
            return;
        }
        let Some(i) = self.prof_state.selected().filter(|&i| i < self.slots.len()) else {
            return;
        };
        let j = i as isize + delta;
        if j < 0 || j as usize >= self.slots.len() {
            return;
        }
        self.slots.swap(i, j as usize);
        self.prof_state.select(Some(j as usize));
        self.dirty = true;
    }

    // -- saving -------------------------------------------------------------

    /// Save the current load order to its profile. The load order lives
    /// only in profiles now — the launcher's moddata is never written.
    fn save(&mut self) -> Result<()> {
        let Some(name) = self.current_profile.clone() else {
            self.status =
                "No profile selected — press p then n to save this load order as one".into();
            return Ok(());
        };
        let enabled = self.slots.iter().filter(|s| !self.slot_missing(s)).count();
        self.status = match self.write_modlist(&name) {
            Ok(vaulted) if vaulted > 0 => {
                format!("Saved '{name}': {enabled} mods, {vaulted} pack versions stored")
            }
            Ok(_) => format!("Saved '{name}': {enabled} mods"),
            Err(e) => format!("Saving '{name}' failed: {e:#}"),
        };
        self.dirty = false;
        Ok(())
    }

    // -- profiles -----------------------------------------------------------

    fn refresh_profiles(&mut self) {
        self.profiles = fs::read_dir(modlists_dir())
            .map(|rd| {
                let mut v: Vec<String> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "json"))
                    .filter_map(|p| p.file_stem()?.to_str().map(String::from))
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default();
        let sel = self
            .profile_list
            .selected()
            .unwrap_or(0)
            .min(self.profiles.len().saturating_sub(1));
        self.profile_list
            .select(if self.profiles.is_empty() { None } else { Some(sel) });
        self.preview_for = None;
    }

    fn selected_profile(&self) -> Option<&str> {
        self.profile_list
            .selected()
            .and_then(|i| self.profiles.get(i))
            .map(String::as_str)
    }

    fn read_modlist(name: &str) -> Result<Vec<MlEntry>> {
        let path = modlists_dir().join(format!("{name}.json"));
        let text = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let root: Value = serde_json::from_str(&text)
            .with_context(|| format!("could not parse {}", path.display()))?;
        let entries = root
            .get("mods")
            .and_then(Value::as_array)
            .context("profile has no \"mods\" array")?;
        Ok(entries
            .iter()
            .filter_map(|e| {
                let steam_id = e.get("steam_id").and_then(Value::as_str).map(String::from);
                Some(MlEntry {
                    id: ml_entry_id(e)?,
                    local: e
                        .get("local")
                        .and_then(Value::as_bool)
                        .unwrap_or_else(|| steam_id.is_none()),
                    steam_id,
                    manifest: e.get("manifest").and_then(Value::as_str).map(String::from),
                    sha256: e.get("sha256").and_then(Value::as_str).map(String::from),
                })
            })
            .collect())
    }

    /// Load the selected profile's mods for the popup's preview pane,
    /// resolving names against the pool.
    fn refresh_preview(&mut self) {
        let name = self.selected_profile().map(String::from);
        if self.preview_for == name {
            return;
        }
        self.preview_for = name.clone();
        self.preview.clear();
        let Some(name) = name else { return };
        for e in Self::read_modlist(&name).unwrap_or_default() {
            let known = self.pool.iter().find(|m| m.id().eq_ignore_ascii_case(&e.id));
            let updated = match (&known, &e.manifest) {
                (Some(m), Some(pinned)) => m
                    .steam_id
                    .as_ref()
                    .and_then(|sid| self.ws.get(sid))
                    .is_some_and(|w| !w.manifest.is_empty() && &w.manifest != pinned),
                _ => false,
            };
            self.preview.push(match known {
                Some(m) => PreviewLine {
                    label: m.name().to_string(),
                    missing: m.missing,
                    updated,
                },
                None => PreviewLine {
                    label: e.id,
                    missing: true,
                    updated: false,
                },
            });
        }
    }

    /// Write the profile with current version pins, vaulting each pinned
    /// pack version that isn't vaulted yet. Returns how many pack
    /// versions were newly vaulted.
    fn write_modlist(&mut self, name: &str) -> Result<usize> {
        // Slots keep their id even when the mod is missing, so missing
        // mods survive a rewrite.
        let mut vaulted = 0usize;
        let mut mods: Vec<Value> = Vec::with_capacity(self.slots.len());
        for s in &self.slots {
            let mut o = serde_json::json!({ "id": s.id, "active": true });
            if let Some(entry) = s.idx.map(|i| &self.pool[i]) {
                o["local"] = Value::from(entry.local);
                if let Some(sid) = &entry.steam_id {
                    o["steam_id"] = Value::from(sid.clone());
                    if let Some(w) = self.ws.get(sid) {
                        o["manifest"] = Value::from(w.manifest.clone());
                        o["timeupdated"] = Value::from(w.timeupdated);
                        o["size"] = Value::from(w.size);
                        vaulted += store_mod_version(sid, &w.manifest, &entry.dir)?;
                        if let Some(hash) = entry
                            .packs
                            .first()
                            .and_then(|p| stored_pack_path(sid, &w.manifest, p))
                            .and_then(|vp| read_or_make_sha256(&vp))
                        {
                            o["sha256"] = Value::from(hash);
                        }
                    }
                }
            }
            mods.push(o);
        }
        let root = serde_json::json!({
            "saved_at": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "game_buildid": self.game_buildid,
            "mods": mods,
        });
        let dir = modlists_dir();
        fs::create_dir_all(&dir)?;
        let text = serde_json::to_string_pretty(&root)?;
        fs::write(dir.join(format!("{name}.json")), text)?;
        // The file now pins what's installed; refresh in-memory pins if
        // it's the current profile.
        if self.current_profile.as_deref() == Some(name) {
            self.load_pins(name);
        }
        Ok(vaulted)
    }

    fn save_profile(&mut self, name: &str) -> Result<()> {
        self.set_current(Some(name.to_string()));
        let vaulted = self.write_modlist(name)?;
        self.status = if vaulted > 0 {
            format!(
                "Saved profile '{name}' ({} mods, {vaulted} pack versions vaulted)",
                self.slots.len()
            )
        } else {
            format!("Saved profile '{name}' ({} mods)", self.slots.len())
        };
        Ok(())
    }

    fn apply_profile(&mut self, name: &str) -> Result<()> {
        // read_modlist errors surface here (bad JSON etc.).
        Self::read_modlist(name)?;
        self.slots = self.slots_for_profile(name);
        let missing = self.slots.iter().filter(|s| self.slot_missing(s)).count();
        self.prof_state
            .select(if self.slots.is_empty() { None } else { Some(0) });
        if self.avail_state.selected().is_none() && !self.available().is_empty() {
            self.avail_state.select(Some(0));
        }
        // The load order now matches the saved profile; nothing unsaved.
        self.dirty = false;
        self.set_current(Some(name.to_string()));
        self.load_pins(name);
        let mut notes = Vec::new();
        if missing > 0 {
            notes.push(format!("{missing} missing (kept in profile, not enabled)"));
        }
        if let Some(drift) = self.drift_report() {
            notes.push(drift);
        }
        let note = if notes.is_empty() {
            String::new()
        } else {
            format!(" — {}", notes.join("; "))
        };
        self.status = format!(
            "Switched to profile '{name}': {} mods{note}",
            self.slots.len()
        );
        Ok(())
    }

    // -- launching ----------------------------------------------------------

    /// The load order resolved to concrete mods, in launch order. Shared
    /// by launch (`write_used_mods`) and the dry-run (`used_mods_preview`)
    /// so the two never drift. Read-only: for a not-yet-vaulted Workshop
    /// version it returns the vault path it *will* load from; the caller
    /// materializes it. Missing mods are skipped.
    fn resolve_load_order(&self) -> Vec<ResolvedMod> {
        let mut out: Vec<ResolvedMod> = Vec::new();
        for s in &self.slots {
            let Some(idx) = s.idx else { continue };
            let entry = &self.pool[idx];
            if entry.missing {
                continue;
            }
            let packs: Vec<String> = entry
                .packs
                .iter()
                .filter_map(|p| p.file_name()?.to_str().map(String::from))
                .collect();
            // Both kinds mirror their whole folder into data/ (packs +
            // preview + any loose files, at matching relative paths). The
            // file list comes from the mod folder; for Workshop mods each
            // file is served from the vault (pinned/current version) so a
            // launch never needs the live workshop folder, for local mods
            // from the folder itself.
            let (manifest, source) = if entry.local {
                (None, PackSource::Local)
            } else {
                self.workshop_source(entry, s)
            };
            let sid = entry.steam_id.clone().unwrap_or_default();
            let files: Vec<(PathBuf, PathBuf)> = walk_files(&entry.dir)
                .into_iter()
                .map(|(rel, live)| {
                    let src = match &manifest {
                        Some(m) => versioned_mods_dir().join(&sid).join(m).join(&rel),
                        None => live,
                    };
                    (rel, src)
                })
                .collect();
            out.push(ResolvedMod {
                name: entry.name().to_string(),
                source,
                files,
                packs,
                idx,
                manifest,
            });
        }
        out
    }

    /// The Workshop version to load: the pinned one when Steam has moved
    /// past it and it's vaulted, else the current manifest (served from
    /// the vault), else None (no manifest known — load live).
    fn workshop_source(&self, entry: &ModEntry, s: &Slot) -> (Option<String>, PackSource) {
        let sid = entry.steam_id.as_deref().unwrap_or("");
        if self.slot_updated(s) {
            if let Some(pin) = self.pins.get(&s.id) {
                if versioned_mods_dir().join(sid).join(pin).is_dir() {
                    return (Some(pin.clone()), PackSource::Pinned);
                }
            }
        }
        match self.ws.get(sid).map(|w| w.manifest.clone()).filter(|m| !m.is_empty()) {
            Some(m) => (Some(m), PackSource::Vault),
            None => (None, PackSource::Workshop),
        }
    }

    /// The staging mirror entries (rel, src) for the resolved order,
    /// deduped so a later mod overrides an earlier one. Each file falls
    /// back to the mod folder when its vault source isn't present.
    fn staging_entries(&self, resolved: &[ResolvedMod]) -> Vec<(PathBuf, PathBuf)> {
        let mut flat: Vec<(PathBuf, PathBuf)> = Vec::new();
        for r in resolved {
            let dir = &self.pool[r.idx].dir;
            for (rel, src) in &r.files {
                let src = if src.exists() { src.clone() } else { dir.join(rel) };
                flat.push((rel.clone(), src));
            }
        }
        dedup_last(flat, |(rel, _)| rel.clone())
    }

    /// used_mods `mod "…";` pack basenames for the resolved order, in load
    /// order, deduped (later wins).
    fn used_mods_lines(resolved: &[ResolvedMod]) -> Vec<String> {
        let flat: Vec<String> = resolved.iter().flat_map(|r| r.packs.clone()).collect();
        dedup_last(flat, |s| s.clone())
    }

    /// Generate used_mods.txt in the game dir from the load order. Each
    /// mod's folder is mirrored into the staging folder (Workshop mods
    /// mirror their packs from the vault, vaulted on demand; local mods
    /// mirror their whole folder), which twwh3-run overlays onto data/ (or
    /// becomes the mod working directory). Because Workshop packs resolve
    /// through the vault, a launch never depends on the live workshop
    /// folder. `ls -lR` on staging shows the resolution. Returns
    /// (mods, pinned).
    fn write_used_mods(&self) -> Result<(usize, usize)> {
        let game_dir = game_install_dir().context(
            "could not find the game install dir (steamapps/common/Total War WARHAMMER III)",
        )?;
        let resolved = self.resolve_load_order();
        // Vault current Workshop versions on demand so their symlinks
        // resolve; pinned ones are already vaulted.
        for r in &resolved {
            if r.source == PackSource::Vault {
                let e = &self.pool[r.idx];
                if let (Some(sid), Some(m)) = (&e.steam_id, &r.manifest) {
                    let _ = store_mod_version(sid, m, &e.dir);
                }
            }
        }
        let entries = self.staging_entries(&resolved);
        let lines = Self::used_mods_lines(&resolved);
        let pinned = resolved.iter().filter(|r| r.source == PackSource::Pinned).count();
        rebuild_staging(&staging_dir(), &entries)?;
        // Two lists: used_mods.txt loads packs from the staging folder (no
        // overlay); used_mods_overlay.txt has mod lines only, for when
        // twwh3-run has merged staging into data/. Loading from both at
        // once would present every pack twice and confuse save-game mod
        // matching, so the shim picks exactly one.
        let mut out = format!("add_working_directory \"{}\";\n", unix_to_win(&staging_dir()));
        let mut overlay_out = String::new();
        for name in &lines {
            out.push_str(&format!("mod \"{name}\";\n"));
            overlay_out.push_str(&format!("mod \"{name}\";\n"));
        }
        fs::write(game_dir.join("used_mods.txt"), out)
            .with_context(|| format!("could not write used_mods.txt in {}", game_dir.display()))?;
        fs::write(game_dir.join("used_mods_overlay.txt"), overlay_out).with_context(|| {
            format!("could not write used_mods_overlay.txt in {}", game_dir.display())
        })?;
        Ok((resolved.len(), pinned))
    }

    /// Render exactly what a launch would pass to the game, without
    /// writing anything or touching the vault. Shares the resolver with
    /// the real launch, so the mod lines and their order are identical.
    fn used_mods_preview(&self) -> String {
        let resolved = self.resolve_load_order();
        let lines = Self::used_mods_lines(&resolved);
        let staging = staging_dir();
        let overlay = setting("TWWH3_OVERLAY", "overlay").unwrap_or_else(|| "on".into());
        let mut s = String::new();
        s.push_str(&format!(
            "Load order — {} mods ({} packs), in launch order:\n",
            resolved.len(),
            lines.len()
        ));
        for (n, r) in resolved.iter().enumerate() {
            let pk = if r.packs.len() == 1 {
                String::new()
            } else {
                format!(", {} packs", r.packs.len())
            };
            s.push_str(&format!("{:>3}  {}  [{}{}]\n", n + 1, r.name, r.source.label(), pk));
        }
        let missing = self.slots.iter().filter(|s| self.slot_missing(s)).count();
        if missing > 0 {
            s.push_str(&format!("     ({missing} missing / not installed — skipped)\n"));
        }
        s.push('\n');
        if overlay == "off" {
            s.push_str("overlay: off → the game is passed used_mods.txt (working-directory loading)\n\n");
        } else {
            s.push_str(
                "overlay: on → the game is passed used_mods_overlay.txt when the fuse mount \
                 succeeds,\n             otherwise it falls back to used_mods.txt\n\n",
            );
        }
        s.push_str("----- used_mods_overlay.txt (packs merged into data/) -----\n");
        for name in &lines {
            s.push_str(&format!("mod \"{name}\";\n"));
        }
        s.push_str("\n----- used_mods.txt (fallback / overlay off) -----\n");
        s.push_str(&format!("add_working_directory \"{}\";\n", unix_to_win(&staging)));
        for name in &lines {
            s.push_str(&format!("mod \"{name}\";\n"));
        }
        s
    }

    /// Write used_mods.txt and ask Steam to start the game (Steam does
    /// the Proton wrapping exactly as if launched from its UI).
    fn launch(&mut self) {
        if self.dirty {
            self.status = "Unsaved changes — press s first, then L to launch".into();
            return;
        }
        let (mods, pinned) = match self.write_used_mods() {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("Launch failed: {e:#}");
                return;
            }
        };
        // A preview mount is now the shim's problem: it unmounts stale
        // overlays before mounting its own.
        self.preview_mounted = false;
        match Command::new("steam")
            .args(["-applaunch", &APPID.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => {
                let pin_note = if pinned > 0 {
                    format!(", {pinned} pinned versions")
                } else {
                    String::new()
                };
                // twwh3-run decides the overlay method a moment later, when
                // the game actually starts, and records it; watch for that
                // report (newer than now) and show it when it lands.
                self.overlay_watch_since = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .ok();
                self.status = format!(
                    "Launching via Steam: {mods} mods{pin_note} — overlay result will appear here when the game starts"
                );
            }
            Err(e) => self.status = format!("could not run steam: {e}"),
        }
    }

    /// After L, surface twwh3-run's definitive overlay decision once it
    /// records one newer than the launch. Called each UI tick.
    fn poll_overlay_status(&mut self) {
        let Some(since) = self.overlay_watch_since else { return };
        let Ok(text) = fs::read_to_string(overlay_status_file()) else { return };
        let Some((ts, msg)) = text.trim().split_once('\t') else { return };
        if ts.parse::<u64>().is_ok_and(|t| t >= since) {
            self.status = msg.to_string();
            self.overlay_watch_since = None;
        }
    }

    /// `o`: open the game's data/ folder as the game will see it. If an
    /// overlay is already mounted (game running), just open it; otherwise
    /// mount a preview of the current load order first. Pressing o again
    /// unmounts a preview we mounted.
    fn toggle_data_view(&mut self) {
        let Some(game) = game_install_dir() else {
            self.status = "Data view: game install dir not found".into();
            return;
        };
        let data = game.join("data");
        if self.preview_mounted {
            let ok = Command::new("fusermount3")
                .args(["-u"])
                .arg(&data)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            self.preview_mounted = false;
            self.status = if ok || !overlay_mounted(&data) {
                "Data preview unmounted".into()
            } else {
                format!("Could not unmount the preview on {}", data.display())
            };
            return;
        }
        if overlay_mounted(&data) {
            self.status = if open_dir(&data) {
                format!(
                    "Opened merged data/ — overlay already mounted; if the game isn't running, \
                     `fusermount3 -u \"{}\"` removes it",
                    tilde(&data)
                )
            } else {
                format!("Data view: {OPENER_HINT}")
            };
            return;
        }
        // Refresh staging (and both mod lists) so the preview shows
        // exactly what a launch would.
        if let Err(e) = self.write_used_mods() {
            self.status = format!("Data view failed: {e:#}");
            return;
        }
        let Some(fo) = find_fuse_overlayfs() else {
            self.status = if open_dir(&data) {
                "fuse-overlayfs not found — opening plain data/ (no merged preview)".into()
            } else {
                format!("Data view: {OPENER_HINT}")
            };
            return;
        };
        let mounted = Command::new(fo)
            .arg("-o")
            .arg(format!(
                "lowerdir={}:{}",
                staging_dir().display(),
                data.display()
            ))
            .arg(&data)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if mounted {
            self.preview_mounted = true;
            self.status = if open_dir(&data) {
                "Mounted merged data/ preview — press o again to unmount before playing".into()
            } else {
                format!("Preview mounted ({OPENER_HINT}); press o again to unmount")
            };
        } else {
            self.status = if open_dir(&data) {
                "Overlay preview mount failed — opening plain data/".into()
            } else {
                format!("Overlay preview mount failed; {OPENER_HINT}")
            };
        }
    }

    fn delete_profile(&mut self, name: &str) -> Result<()> {
        fs::remove_file(modlists_dir().join(format!("{name}.json")))?;
        if self.current_profile.as_deref() == Some(name) {
            self.set_current(None);
        }
        self.status = format!("Deleted profile '{name}'");
        Ok(())
    }

    fn rename_profile(&mut self, from: &str, to: &str) -> Result<()> {
        if to == from {
            return Ok(());
        }
        let dir = modlists_dir();
        let dst = dir.join(format!("{to}.json"));
        if dst.exists() {
            bail!("a profile named '{to}' already exists");
        }
        fs::rename(dir.join(format!("{from}.json")), &dst)
            .with_context(|| format!("could not rename '{from}' to '{to}'"))?;
        if self.current_profile.as_deref() == Some(from) {
            self.set_current(Some(to.to_string()));
        }
        self.status = format!("Renamed profile '{from}' → '{to}'");
        Ok(())
    }

    // -- portable bundles ---------------------------------------------------

    /// Pack profile `name` and all of its vaulted packs into a single
    /// self-contained `<name>.twwh3bundle.tar` under `dest_dir`, so a mod
    /// setup can be moved to another machine or shared. Returns
    /// (archive path, packs included, mods without a vaulted pack).
    fn export_bundle(name: &str, dest_dir: &Path) -> Result<(PathBuf, usize, usize)> {
        let entries = Self::read_modlist(name)?;
        let profile_path = modlists_dir().join(format!("{name}.json"));
        if !profile_path.exists() {
            bail!("no such profile: {name}");
        }
        fs::create_dir_all(dest_dir)
            .with_context(|| format!("could not create {}", dest_dir.display()))?;
        let out = dest_dir.join(format!("{name}.twwh3bundle.tar"));
        let tmp = dest_dir.join(format!(".{name}.twwh3bundle.tar.tmp"));
        let mut tar = tar::Builder::new(fs::File::create(&tmp)?);

        // Header: schema marker + the pinned version of every mod.
        let header = serde_json::json!({
            "schema": 1,
            "profile": name,
            "mods": entries.iter().map(|e| serde_json::json!({
                "id": e.id,
                "local": e.local,
                "steam_id": e.steam_id,
                "manifest": e.manifest,
                "sha256": e.sha256,
            })).collect::<Vec<_>>(),
        });
        tar_append_bytes(&mut tar, "bundle.json", &serde_json::to_vec_pretty(&header)?)?;
        // The profile file itself, applied verbatim on import.
        tar.append_path_with_name(&profile_path, "profile.json")?;

        // Each mod's files: Workshop from its vaulted version (pack, png,
        // sidecar); local mods from their whole folder.
        let vault = versioned_mods_dir();
        let mut packs = 0usize;
        let mut missing = 0usize;
        for e in &entries {
            if e.local {
                let src = local_mods_dir().join(&e.id);
                let mut has_pack = false;
                for (rel, abs) in walk_files(&src) {
                    tar.append_path_with_name(&abs, format!("local/{}/{}", e.id, rel.display()))?;
                    if abs.extension().is_some_and(|x| x.eq_ignore_ascii_case("pack")) {
                        has_pack = true;
                    }
                }
                if has_pack {
                    packs += 1;
                } else {
                    missing += 1;
                }
                continue;
            }
            let (Some(sid), Some(manifest)) = (&e.steam_id, &e.manifest) else {
                missing += 1;
                continue;
            };
            let dir = vault.join(sid).join(manifest);
            let mut has_pack = false;
            for f in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = f.path();
                if !p.is_file() {
                    continue;
                }
                let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
                tar.append_path_with_name(&p, format!("packs/{sid}/{manifest}/{fname}"))?;
                if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pack")) {
                    has_pack = true;
                }
            }
            if has_pack {
                packs += 1;
            } else {
                missing += 1;
            }
        }
        tar.finish()?;
        drop(tar);
        fs::rename(&tmp, &out)?;
        Ok((out, packs, missing))
    }

    /// Import a bundle produced by `export_bundle`: extract its packs into
    /// the vault (verifying each against the recorded sha256) and install
    /// its profile under `modlists/`, without overwriting an existing one.
    /// Returns (profile name it was saved as, packs verified).
    fn import_bundle(path: &Path, as_name: Option<&str>) -> Result<(String, usize)> {
        let vault = versioned_mods_dir();
        fs::create_dir_all(&vault)?;
        let file =
            fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
        let mut ar = tar::Archive::new(file);
        let mut profile_json: Option<Vec<u8>> = None;
        let mut header: Option<Value> = None;
        for entry in ar.entries()? {
            let mut entry = entry?;
            let arc = entry.path()?.into_owned();
            let name = arc.to_string_lossy().to_string();
            if name == "bundle.json" {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                header = serde_json::from_slice(&buf).ok();
            } else if name == "profile.json" {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                profile_json = Some(buf);
            } else if let Ok(rel) = arc.strip_prefix("packs") {
                // packs/<sid>/<manifest>/<file> -> vault/<sid>/<manifest>/<file>
                if rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                    bail!("bundle contains an unsafe path: {name}");
                }
                // Bare directory entries (e.g. "packs/") carry no file.
                if rel.as_os_str().is_empty() || entry.header().entry_type().is_dir() {
                    continue;
                }
                let dst = vault.join(rel);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                entry.unpack(&dst)?;
            } else if let Ok(rel) = arc.strip_prefix("local") {
                // local/<id>/<file> -> <local mods dir>/<id>/<file>
                if rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                    bail!("bundle contains an unsafe path: {name}");
                }
                if rel.as_os_str().is_empty() || entry.header().entry_type().is_dir() {
                    continue;
                }
                let dst = local_mods_dir().join(rel);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                entry.unpack(&dst)?;
            }
        }
        let Some(profile_json) = profile_json else {
            bail!("not a twwh3 bundle (no profile.json)");
        };

        // Verify each extracted pack against the profile's recorded hash.
        let root: Value = serde_json::from_slice(&profile_json)?;
        let mut verified = 0usize;
        for m in root.get("mods").and_then(Value::as_array).into_iter().flatten() {
            let (Some(sid), Some(manifest), Some(sha)) = (
                m.get("steam_id").and_then(Value::as_str),
                m.get("manifest").and_then(Value::as_str),
                m.get("sha256").and_then(Value::as_str),
            ) else {
                continue;
            };
            let Some(pack) = find_stored_pack(&vault.join(sid).join(manifest)) else {
                continue;
            };
            if let Some(actual) = pack_sha256(&pack) {
                if actual != sha {
                    bail!(
                        "checksum mismatch for {} — bundle may be corrupt",
                        pack.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
                verified += 1;
            }
        }

        // Pick a non-colliding profile name and install the profile file.
        let base = as_name
            .map(str::to_string)
            .or_else(|| {
                header
                    .as_ref()
                    .and_then(|h| h.get("profile").and_then(Value::as_str))
                    .map(String::from)
            })
            .or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.trim_end_matches(".tar").trim_end_matches(".twwh3bundle").to_string())
            })
            .unwrap_or_else(|| "imported".into());
        if !valid_profile_name(&base) {
            bail!("invalid profile name derived from bundle: '{base}' (use --as <name>)");
        }
        let dir = modlists_dir();
        fs::create_dir_all(&dir)?;
        let name = unique_profile_name(&dir, base.trim());
        fs::write(dir.join(format!("{name}.json")), &profile_json)?;
        Ok((name, verified))
    }
}

fn valid_profile_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        && name != "."
        && name != ".."
}

// ---------------------------------------------------------------------------
// UI

fn draw(f: &mut Frame, app: &mut App) {
    let [header, main, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [avail_area, prof_area, side_area] = Layout::horizontal([
        Constraint::Min(30),
        Constraint::Min(34),
        Constraint::Length(44),
    ])
    .areas(main);

    // Profile front and center on the left; the moddata path (dim,
    // head-truncated) right-aligned in whatever space is left.
    let mut spans = vec![
        Span::styled(" twwh3-mods ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("· profile: ", Style::default().fg(Color::DarkGray)),
        profile_span(app),
    ];
    if app.dirty {
        spans.push(Span::styled(
            "  [modified]",
            Style::default().fg(Color::Yellow),
        ));
    }
    let left = Line::from(spans);
    let left_width = left.width() as u16;
    f.render_widget(Paragraph::new(left), header);

    let pad = left_width + 2;
    if header.width > pad + 12 {
        let path_area = Rect::new(header.x + pad, header.y, header.width - pad, 1);
        let path = app.path.display().to_string();
        let home = env::var("HOME").unwrap_or_default();
        let path = if home.is_empty() {
            path
        } else {
            path.replacen(&home, "~", 1)
        };
        let room = path_area.width as usize - 1;
        let shown = if path.chars().count() > room {
            let tail: String = path
                .chars()
                .rev()
                .take(room.saturating_sub(1))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("…{tail} ")
        } else {
            format!("{path} ")
        };
        f.render_widget(
            Paragraph::new(shown)
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Right),
            path_area,
        );
    }

    draw_available(f, app, avail_area);
    draw_profile(f, app, prof_area);
    draw_side_panel(f, app, side_area);

    let help_line = if !app.status.is_empty() {
        Line::from(format!(" {}", app.status)).style(Style::default().fg(Color::Yellow))
    } else {
        // Full controls live in the ? modal; the bar just points to it.
        let keys = match app.mode {
            Mode::Browse => " ? help · q quit",
            Mode::Profiles => " enter apply · r rename · n new · e export · d delete · esc close",
            Mode::NameInput => " enter confirm · esc cancel",
            Mode::Status | Mode::Help => " esc close",
        };
        Line::from(keys).style(Style::default().fg(Color::DarkGray))
    };
    f.render_widget(Paragraph::new(help_line), help);

    match app.mode {
        Mode::Profiles => draw_profiles_popup(f, app),
        Mode::NameInput => draw_name_input(f, app),
        Mode::Status => draw_status_popup(f, app),
        Mode::Help => draw_help_popup(f),
        Mode::Browse => {}
    }
}

/// All keybindings, grouped, shown by `?`.
fn draw_help_popup(f: &mut Frame) {
    // (key, description); empty key = section header, empty both = spacer.
    let rows: &[(&str, &str)] = &[
        ("Navigation", ""),
        ("tab / h / l", "switch pane"),
        ("j / k / ↑ / ↓", "move selection"),
        ("g / G", "jump to top / bottom"),
        ("", ""),
        ("Load order", ""),
        ("space / enter", "add to / remove from the load order"),
        ("J / K", "reorder the selected mod"),
        ("", ""),
        ("Profiles (p)", ""),
        ("enter", "apply the selected profile"),
        ("n", "new profile from the current load order"),
        ("r", "rename the selected profile"),
        ("e", "export the profile as a portable bundle"),
        ("d", "delete (press twice to confirm)"),
        ("", ""),
        ("Actions", ""),
        ("s", "save load order + profile"),
        ("S", "status page (paths, launch plumbing)"),
        ("o", "open game data/ as the game sees it (merged preview)"),
        ("L", "write used_mods.txt and launch via Steam"),
        ("? ", "this help"),
        ("q / esc", "quit (or close a popup)"),
    ];
    let full = f.area();
    let h = (rows.len() as u16 + 2).min(full.height.saturating_sub(2));
    let area = centered_rect(full.width.saturating_sub(6).min(72), h, full);
    f.render_widget(Clear, area);
    let table_rows: Vec<Row> = rows
        .iter()
        .map(|(key, desc)| {
            if desc.is_empty() {
                Row::new(vec![Cell::from(*key).style(
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )])
            } else {
                Row::new(vec![
                    Cell::from(format!("  {key}"))
                        .style(Style::default().fg(Color::Yellow)),
                    Cell::from(*desc),
                ])
            }
        })
        .collect();
    let table = Table::new(table_rows, [Constraint::Length(16), Constraint::Min(20)]).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Keys ")
            .title_bottom(" esc close "),
    );
    f.render_widget(table, area);
}

fn draw_status_popup(f: &mut Frame, app: &App) {
    let full = f.area();
    let h = (app.status_lines.len() as u16 + 2).min(full.height.saturating_sub(2));
    let area = centered_rect(full.width.saturating_sub(6).min(100), h, full);
    f.render_widget(Clear, area);
    let rows: Vec<Row> = app
        .status_lines
        .iter()
        .map(|l| {
            if l.value.is_empty() {
                Row::new(vec![Cell::from(l.label.clone()).style(
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )])
            } else {
                let style = match l.ok {
                    Some(true) => Style::default().fg(Color::Green),
                    Some(false) => Style::default().fg(Color::Red),
                    None => Style::default().fg(Color::DarkGray),
                };
                Row::new(vec![
                    Cell::from(format!("  {}", l.label)),
                    Cell::from(l.value.clone()).style(style),
                ])
            }
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(17), Constraint::Min(20)]).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Status ")
            .title_bottom(" esc close "),
    );
    f.render_widget(table, area);
}

/// The current profile name, styled so it stands out (yellow "(unsaved)"
/// when the load order isn't attached to a profile yet).
fn profile_span(app: &App) -> Span<'static> {
    match &app.current_profile {
        Some(p) => Span::styled(
            p.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        None => Span::styled("(unsaved)", Style::default().fg(Color::Yellow)),
    }
}

fn pane_block(title: Line<'static>, focused: bool) -> Block<'static> {
    let block = Block::default().borders(Borders::ALL).title(title);
    if focused {
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block
    }
}

fn highlight(focused: bool) -> (Style, &'static str) {
    if focused {
        (Style::default().add_modifier(Modifier::REVERSED), "▶ ")
    } else {
        (Style::default().add_modifier(Modifier::UNDERLINED), "  ")
    }
}

fn draw_available(f: &mut Frame, app: &mut App, area: Rect) {
    let avail = app.available();
    let rows: Vec<Row> = avail
        .iter()
        .map(|&i| {
            let m = &app.pool[i];
            let name_style = if m.local {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default()
            };
            let label = if m.packs.len() > 1 {
                format!("{} ({} packs)", m.name(), m.packs.len())
            } else {
                m.name().to_string()
            };
            Row::new(vec![
                Cell::from(label).style(name_style),
                Cell::from(format!(
                    "{:>9}",
                    m.size.map(human_size).unwrap_or_else(|| "-".into())
                ))
                .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    let focused = app.focus == Pane::Available;
    let (hl, sym) = highlight(focused);
    let table = Table::new(rows, [Constraint::Min(20), Constraint::Length(9)])
        .block(pane_block(
            Line::from(format!(" Available ({}) ", avail.len())),
            focused,
        ))
        .row_highlight_style(hl)
        .highlight_symbol(sym);
    f.render_stateful_widget(table, area, &mut app.avail_state);
}

fn draw_profile(f: &mut Frame, app: &mut App, area: Rect) {
    let rows: Vec<Row> = app
        .slots
        .iter()
        .enumerate()
        .map(|(n, s)| {
            let (label, style) = match s.idx {
                Some(i) if !app.pool[i].missing => {
                    if app.slot_updated(s) {
                        (
                            format!("{} (updated)", app.pool[i].name()),
                            Style::default().fg(Color::Yellow),
                        )
                    } else {
                        (app.pool[i].name().to_string(), Style::default())
                    }
                }
                Some(i) => (
                    format!("{} (missing)", app.pool[i].name()),
                    Style::default().fg(Color::Red),
                ),
                None => (
                    format!("{} (missing)", s.id),
                    Style::default().fg(Color::Red),
                ),
            };
            Row::new(vec![
                Cell::from(format!("{:>3}", n + 1)).style(Style::default().fg(Color::Yellow)),
                Cell::from(label).style(style),
            ])
        })
        .collect();
    let focused = app.focus == Pane::Profile;
    let (hl, sym) = highlight(focused);
    let title = Line::from(vec![
        Span::raw(" Load order — "),
        profile_span(app),
        Span::raw(format!(" ({}) ", app.slots.len())),
    ]);
    let table = Table::new(rows, [Constraint::Length(3), Constraint::Min(20)])
        .block(pane_block(title, focused))
        .row_highlight_style(hl)
        .highlight_symbol(sym);
    f.render_stateful_widget(table, area, &mut app.prof_state);
}

/// The pool index of the mod selected in the focused pane, or the bare
/// uuid for a profile entry that isn't installed.
enum SidePanelSubject {
    Mod(usize),
    Unknown(String),
    None,
}

fn side_panel_subject(app: &App) -> SidePanelSubject {
    match app.focus {
        Pane::Available => {
            let avail = app.available();
            match app.avail_state.selected().filter(|&s| s < avail.len()) {
                Some(s) => SidePanelSubject::Mod(avail[s]),
                None => SidePanelSubject::None,
            }
        }
        Pane::Profile => match app
            .prof_state
            .selected()
            .filter(|&s| s < app.slots.len())
            .map(|s| &app.slots[s])
        {
            Some(slot) => match slot.idx {
                Some(i) => SidePanelSubject::Mod(i),
                None => SidePanelSubject::Unknown(slot.id.clone()),
            },
            None => SidePanelSubject::None,
        },
    }
}

fn draw_side_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Mod ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let i = match side_panel_subject(app) {
        SidePanelSubject::Mod(i) => i,
        SidePanelSubject::Unknown(uuid) => {
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(uuid),
                    Line::from("missing — not installed (unsubscribed?)")
                        .style(Style::default().fg(Color::Red)),
                ])
                .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        SidePanelSubject::None => {
            f.render_widget(Paragraph::new("nothing selected"), inner);
            return;
        }
    };
    let [img_area, text_area] =
        Layout::vertical([Constraint::Length(12), Constraint::Min(1)]).areas(inner);

    // Load the thumbnail lazily, once per mod.
    let App { pool, picker, .. } = app;
    let entry = &mut pool[i];
    if matches!(entry.thumb, Thumb::NotLoaded) {
        entry.thumb = match picker {
            Some(picker) => entry
                .png
                .as_ref()
                .and_then(|p| image::ImageReader::open(p).ok()?.decode().ok())
                .map(|img| Thumb::Ready(picker.new_resize_protocol(img)))
                .unwrap_or(Thumb::Missing),
            None => Thumb::Missing,
        };
    }
    match &mut entry.thumb {
        Thumb::Ready(proto) => {
            f.render_stateful_widget(StatefulImage::default(), img_area, proto)
        }
        _ => f.render_widget(
            Paragraph::new("(no image)")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            img_area,
        ),
    }

    let entry = &app.pool[i];
    let label = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(Span::styled(
            entry.name().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("kind: ", label),
            Span::raw(if entry.local { "local" } else { "workshop" }.to_string()),
        ]),
        Line::from(vec![
            Span::styled("category: ", label),
            Span::raw(entry.category().to_string()),
        ]),
        Line::from(vec![
            Span::styled("steam id: ", label),
            Span::raw(entry.steam_id.clone().unwrap_or_else(|| "-".into())),
        ]),
        Line::from(vec![
            Span::styled("folder: ", label),
            Span::raw(tilde(&entry.dir)),
        ]),
        Line::from(vec![
            Span::styled("packs: ", label),
            Span::raw(entry.packs.len().to_string()),
        ]),
    ];
    for p in entry.packs.iter().take(8) {
        if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
            lines.push(Line::from(format!("  {n}")).style(label));
        }
    }
    lines.push(Line::from(""));
    if entry.missing {
        lines.insert(
            1,
            Line::from("missing — folder not found").style(Style::default().fg(Color::Red)),
        );
    } else if entry.local {
        lines.insert(
            1,
            Line::from("local mod — a folder mirrored into data/")
                .style(Style::default().fg(Color::Magenta)),
        );
    }
    for l in entry.description().lines().take(12) {
        lines.push(Line::from(l.to_string()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), text_area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn draw_profiles_popup(f: &mut Frame, app: &mut App) {
    app.refresh_preview();
    let full = f.area();
    let area = centered_rect(full.width.saturating_sub(8).min(96), 22, full);
    f.render_widget(Clear, area);
    let [names_area, preview_area] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(24)]).areas(area);

    let items: Vec<ListItem> = if app.profiles.is_empty() {
        vec![ListItem::new("(none yet — press n)")
            .style(Style::default().fg(Color::DarkGray))]
    } else {
        app.profiles
            .iter()
            .map(|p| {
                if app.current_profile.as_deref() == Some(p.as_str()) {
                    ListItem::new(format!("{p} (current)"))
                        .style(Style::default().add_modifier(Modifier::BOLD))
                } else {
                    ListItem::new(p.clone())
                }
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Profiles ")
                .title_bottom(" n new · r rename · e export · d delete "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, names_area, &mut app.profile_list);

    let n_missing = app.preview.iter().filter(|l| l.missing).count();
    let title = if app.preview.is_empty() {
        " Mods in profile ".to_string()
    } else if n_missing > 0 {
        format!(" Mods in profile ({}, {n_missing} missing) ", app.preview.len())
    } else {
        format!(" Mods in profile ({}) ", app.preview.len())
    };
    let lines: Vec<Line> = app
        .preview
        .iter()
        .enumerate()
        .map(|(n, l)| {
            if l.missing {
                Line::from(format!("{:>3} {} (missing)", n + 1, l.label))
                    .style(Style::default().fg(Color::Red))
            } else if l.updated {
                Line::from(format!("{:>3} {} (updated)", n + 1, l.label))
                    .style(Style::default().fg(Color::Yellow))
            } else {
                Line::from(format!("{:>3} {}", n + 1, l.label))
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_bottom(" enter apply · esc close "),
        ),
        preview_area,
    );
}

fn draw_name_input(f: &mut Frame, app: &mut App) {
    let area = centered_rect(44, 3, f.area());
    f.render_widget(Clear, area);
    let title = match &app.rename_from {
        Some(from) => format!(" Rename '{from}' to "),
        None => " New profile name ".to_string(),
    };
    f.render_widget(
        Paragraph::new(format!("{}▏", app.input)).block(
            Block::default().borders(Borders::ALL).title(title),
        ),
        area,
    );
}

// ---------------------------------------------------------------------------
// Event loop

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        // Pick up twwh3-run's overlay report (written asynchronously after
        // a launch) so it can be shown even without a keypress.
        app.poll_overlay_status();
        terminal.draw(|f| draw(f, app))?;
        // Poll rather than block so the overlay report surfaces promptly.
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        let was_confirm_quit = app.confirm_quit;
        let was_confirm_delete = app.confirm_delete;
        app.confirm_quit = false;
        app.confirm_delete = false;
        app.status.clear();

        match app.mode {
            Mode::Browse => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        if !app.dirty || was_confirm_quit {
                            return Ok(());
                        }
                        app.confirm_quit = true;
                        app.status =
                            "Unsaved changes — press q again to discard them, or s to save".into();
                    }
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                        app.switch_pane()
                    }
                    KeyCode::Char('h') | KeyCode::Char('l') => app.switch_pane(),
                    KeyCode::Up if shift => app.move_slot(-1),
                    KeyCode::Down if shift => app.move_slot(1),
                    KeyCode::Char('K') => app.move_slot(-1),
                    KeyCode::Char('J') => app.move_slot(1),
                    KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                    KeyCode::Home | KeyCode::Char('g') => app.select_edge(false),
                    KeyCode::End | KeyCode::Char('G') => app.select_edge(true),
                    KeyCode::Char(' ') | KeyCode::Enter => match app.focus {
                        Pane::Available => app.add_selected(),
                        Pane::Profile => app.remove_selected(),
                    },
                    KeyCode::Char('p') => {
                        app.refresh_profiles();
                        app.mode = Mode::Profiles;
                    }
                    KeyCode::Char('s') => {
                        if let Err(e) = app.save() {
                            app.status = format!("Save failed: {e:#}");
                        }
                    }
                    KeyCode::Char('S') => {
                        app.status_lines = app.build_status();
                        app.mode = Mode::Status;
                    }
                    KeyCode::Char('o') => app.toggle_data_view(),
                    KeyCode::Char('L') => app.launch(),
                    KeyCode::Char('?') => app.mode = Mode::Help,
                    _ => {}
                }
            }
            Mode::Help => {
                app.mode = Mode::Browse;
            }
            Mode::Status => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('S') | KeyCode::Enter
                | KeyCode::Char(' ') => {
                    app.mode = Mode::Browse;
                }
                _ => {}
            },
            Mode::Profiles => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('p') => {
                    app.mode = Mode::Browse;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = app.profile_list.selected().unwrap_or(0);
                    app.profile_list.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !app.profiles.is_empty() {
                        let i = app.profile_list.selected().unwrap_or(0);
                        app.profile_list
                            .select(Some((i + 1).min(app.profiles.len() - 1)));
                    }
                }
                KeyCode::Enter => {
                    if let Some(name) = app.selected_profile().map(String::from) {
                        if let Err(e) = app.apply_profile(&name) {
                            app.status = format!("Could not apply '{name}': {e:#}");
                        }
                        app.mode = Mode::Browse;
                    }
                }
                KeyCode::Char('n') => {
                    app.input.clear();
                    app.rename_from = None;
                    app.mode = Mode::NameInput;
                }
                KeyCode::Char('r') => {
                    if let Some(name) = app.selected_profile().map(String::from) {
                        app.rename_from = Some(name.clone());
                        app.input = name;
                        app.mode = Mode::NameInput;
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(name) = app.selected_profile().map(String::from) {
                        let dest = data_dir().join("bundles");
                        match App::export_bundle(&name, &dest) {
                            Ok((path, packs, missing)) => {
                                let miss = if missing > 0 {
                                    format!(", {missing} without a stored pack")
                                } else {
                                    String::new()
                                };
                                app.status = format!(
                                    "Exported '{name}' → {} ({packs} packs{miss})",
                                    tilde(&path)
                                );
                            }
                            Err(e) => app.status = format!("Export failed: {e:#}"),
                        }
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(name) = app.selected_profile().map(String::from) {
                        if was_confirm_delete {
                            if let Err(e) = app.delete_profile(&name) {
                                app.status = format!("Could not delete '{name}': {e:#}");
                            }
                            app.refresh_profiles();
                        } else {
                            app.confirm_delete = true;
                            app.status = format!("Press d again to delete profile '{name}'");
                        }
                    }
                }
                _ => {}
            },
            Mode::NameInput => match key.code {
                KeyCode::Esc => {
                    app.rename_from = None;
                    app.mode = Mode::Profiles;
                }
                KeyCode::Enter => {
                    let name = app.input.trim().to_string();
                    if !valid_profile_name(&name) {
                        app.status =
                            "Profile names: letters, digits, space, - _ . only".into();
                    } else {
                        let result = match app.rename_from.take() {
                            Some(from) => app.rename_profile(&from, &name),
                            None => app.save_profile(&name),
                        };
                        if let Err(e) = result {
                            app.status = format!("Could not update profile: {e:#}");
                        }
                        app.refresh_profiles();
                        if let Some(i) = app.profiles.iter().position(|p| *p == name) {
                            app.profile_list.select(Some(i));
                        }
                        app.mode = Mode::Profiles;
                    }
                }
                KeyCode::Backspace => {
                    app.input.pop();
                }
                KeyCode::Char(c) if app.input.len() < 40 => app.input.push(c),
                _ => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------

fn usage() {
    println!(
        "twwh3-mods — TUI mod load-order manager for Total War: WARHAMMER III\n\n\
         Usage: twwh3-mods [--list | --launch | --paths | used-mods]\n         \
                twwh3-mods export <profile> [dest-dir]\n         \
                twwh3-mods import <bundle.tar> [--as name]\n\n\
         Options:\n  \
           -l, --list   Print the load order and available mods, then exit\n  \
           --launch     Write used_mods.txt and start the game via Steam\n  \
           --paths      Print every resolved path and where config is read from\n  \
           used-mods    Dry run: print the exact ordered load order and the\n               \
           used_mods.txt / used_mods_overlay.txt a launch would pass to the\n               \
           game — no files written, nothing launched\n  \
           export       Pack a profile + its stored packs into one .tar\n  \
           import       Unpack a bundle into the store and install its profile\n  \
           -h, --help   Show this help\n\n\
         Keys:\n  \
           tab / h / l      switch pane   j/k or arrows        select\n  \
           space / enter    add to or remove from the load order\n  \
           J/K              reorder within the load order\n  \
           p                profiles (enter apply, n new, r rename,\n                    \
           e export, d delete)\n  \
           o                open the game's data/ folder as the game will\n                    \
           see it (mounts a merged preview; o again unmounts)\n  \
           ? help     s save     S status page     L launch     q quit\n\n\
         Launching (one-time setup):\n  \
           L / --launch write used_mods.txt into the game folder and run\n  \
           `steam -applaunch {APPID}` — Steam still does all the Proton work.\n  \
           To make the game (and Steam's own Play button) skip the CA\n  \
           launcher and use that exact file, set the game's Steam launch\n  \
           options to:\n\n    \
           twwh3-run %command%\n\n  \
           (twwh3-run ships alongside this tool.) Without it, the CA\n  \
           launcher opens as usual and uses the same mod list, minus\n  \
           version pinning.\n\n\
         Mods are folders:\n  \
           A mod is a folder mirrored into the game's data/. Workshop items\n  \
           (workshop/content/<appid>/<id>/) and local mods each are folders\n  \
           holding one or more .pack files (+ loose assets for local mods).\n  \
           For a local mod, make a subfolder of the local_mods dir\n  \
           (TWWH3_LOCAL) — e.g. local_mods/MyMod/ — put its .pack(s) at the\n  \
           folder root and any loose files (movies/, etc.) in matching\n  \
           subdirs;\n  \
           it appears under Available marked 'local'. No Steam Workshop\n  \
           needed. The load order lives in profiles; moddata.dat is read for\n  \
           Workshop names but never written (launch is overlay-only).\n\n\
         Versioning (Workshop only):\n  \
           Profiles pin each Workshop mod's Steam manifest (depot GID for an\n  \
           exact version) plus a sha256 and the game build id at save time;\n  \
           the item is copied into versioned_workshop_mods (keyed by id +\n  \
           manifest). When Steam force-updates a mod, the load order marks\n  \
           it '(updated)' and L loads the pinned version from that store.\n  \
           Local mods aren't versioned (you own them).\n\n\
         Staging & overlay:\n  \
           On launch each mod folder is mirrored into a staging folder of\n  \
           symlinks (default: ~/Games/TotalWarWH3/staging) — Workshop files\n  \
           resolve through versioned_workshop_mods (current version, stored\n  \
           on demand, or the pinned one); local folders mirror as-is.\n  \
           `ls -lR` there shows the resolution. Because Workshop files\n  \
           resolve through that store,\n  \
           a launch never depends on the live workshop folder and an\n  \
           unsubscribed mod still plays.\n  \
           twwh3-run then merges the staging folder into the game's data/\n  \
           with fuse-overlayfs for the duration of the run (movie packs\n  \
           work; game files stay pristine). Without fuse-overlayfs it\n  \
           falls back to plain working-directory loading automatically.\n  \
           On a sandboxed (bwrap) Steam, twwh3-run can't mount the overlay\n  \
           itself; while the TUI is open it services the mount from outside\n  \
           the sandbox (launch with L, or keep twwh3-mods running).\n\n\
         Portable bundles:\n  \
           `export <profile>` writes <profile>.twwh3bundle.tar with the\n  \
           profile, its versioned Workshop packs, and its local mod folders\n  \
           (in the TUI, press e in the profiles popup — it lands in\n  \
           <data_dir>/bundles). Move or share that one file;\n  \
           `import <bundle.tar>` unpacks it back (verifying each pack against\n  \
           its sha256) and installs\n  \
           the profile without overwriting an existing one (--as renames).\n\n\
         Configuration (~/.config/twwh3-mods/config, `key = value` lines):\n  \
           steam_root  Steam library containing the game (default: ~/.local/share/Steam)\n  \
           data_dir    Base for this tool's data (default: ~/Games/TotalWarWH3)\n  \
           modlists    Profiles          (default: <data_dir>/modlists)\n  \
           versioned_workshop_mods  Cached Workshop mod versions\n                                    \
           (default: <data_dir>/versioned_workshop_mods)\n  \
           local_mods  Non-Workshop mod folders (default: <data_dir>/local_mods)\n  \
           staging     Launch symlinks   (default: <data_dir>/staging)\n  \
           moddata     Launcher mod list file    (default: derived from steam_root)\n  \
           workshop    Workshop content dir      (default: derived from steam_root)\n  \
           game_dir    Game install dir          (default: derived from steam_root)\n  \
           images      auto (default) | halfblocks | off\n  \
           overlay     on (default) | off — twwh3-run's data/ overlay\n  \
           open_with   command `o` opens folders with (default: xdg-open)\n\n  \
           Each key also has an env var that overrides it: STEAM_ROOT,\n  \
           TWWH3_DATA, TWWH3_MODLISTS, TWWH3_VERSIONED_WORKSHOP_MODS, TWWH3_LOCAL,\n  \
           TWWH3_STAGING, TWWH3_MODDATA, TWWH3_WORKSHOP, TWWH3_GAME,\n  \
           TWWH3_IMAGES, TWWH3_OVERLAY, TWWH3_OPEN. `twwh3-mods --paths`\n  \
           shows the resolved values."
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }

    // Bundle subcommands take positional args, so handle them before the
    // generic flag check below.
    match args.first().map(String::as_str) {
        Some("export") => {
            let name = args
                .get(1)
                .context("usage: twwh3-mods export <profile> [dest-dir]")?;
            let dest = args
                .get(2)
                .map(|s| expand_path(s))
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let (path, packs, missing) = App::export_bundle(name, &dest)?;
            let miss = if missing > 0 {
                format!(", {missing} without a stored pack")
            } else {
                String::new()
            };
            println!("Exported '{name}' → {} ({packs} packs{miss})", path.display());
            return Ok(());
        }
        Some("import") => {
            let file = args
                .get(1)
                .context("usage: twwh3-mods import <bundle.tar> [--as name]")?;
            let as_name = args
                .iter()
                .position(|a| a == "--as")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            let (name, verified) = App::import_bundle(&expand_path(file), as_name)?;
            println!(
                "Imported profile '{name}' ({verified} packs verified). \
                 Run twwh3-mods and press p to apply it."
            );
            return Ok(());
        }
        Some("used-mods") => {
            // Dry run: print the exact load order + used_mods contents a
            // launch would pass, without writing anything.
            let app = App::load(moddata_path(), None)?;
            print!("{}", app.used_mods_preview());
            return Ok(());
        }
        _ => {}
    }

    if let Some(bad) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "-l" | "--list" | "--launch" | "--paths"))
    {
        usage();
        bail!("unknown argument: {bad}");
    }

    if args.iter().any(|a| a == "--paths") {
        let cfg = config_file();
        let cfg_note = if cfg.exists() { "" } else { "  (not present)" };
        println!("config file:  {}{cfg_note}", cfg.display());
        println!("steam_root:   {}", steam_root().display());
        println!("data_dir:     {}", data_dir().display());
        println!("moddata:      {}", moddata_path().display());
        println!("workshop:     {}", workshop_dir().display());
        match game_install_dir() {
            Some(d) => println!("game_dir:     {}", d.display()),
            None => println!("game_dir:     (not found)"),
        }
        println!("modlists:     {}", modlists_dir().display());
        println!("versioned_workshop_mods: {}", versioned_mods_dir().display());
        println!("local_mods:   {}", local_mods_dir().display());
        println!("staging:      {}", staging_dir().display());
        println!(
            "images:       {}",
            setting("TWWH3_IMAGES", "images").unwrap_or_else(|| "auto".into())
        );
        return Ok(());
    }

    if args.iter().any(|a| a == "--launch") {
        let mut app = App::load(moddata_path(), None)?;
        app.launch();
        println!("{}", app.status);
        return Ok(());
    }

    if !args.is_empty() {
        // --list: no terminal queries, no TUI.
        let app = App::load(moddata_path(), None)?;
        let mut out = String::from("Load order:\n");
        for (n, s) in app.slots.iter().enumerate() {
            let (name, note) = match s.idx {
                Some(i) if !app.pool[i].missing => (
                    app.pool[i].name(),
                    if app.slot_updated(s) { "  (updated)" } else { "" },
                ),
                Some(i) => (app.pool[i].name(), "  (missing)"),
                None => (s.id.as_str(), "  (missing)"),
            };
            out.push_str(&format!("{:>3}  {name}{note}\n", n + 1));
        }
        out.push_str("\nAvailable:\n");
        for i in app.available() {
            let m = &app.pool[i];
            let note = if m.local { "  (local)" } else { "" };
            out.push_str(&format!("     {}{note}\n", m.name()));
        }
        // Ignore broken pipes from e.g. `--list | head`.
        use std::io::Write;
        let _ = std::io::stdout().write_all(out.as_bytes());
        return Ok(());
    }

    // Query the terminal for its graphics protocol (kitty/sixel/iTerm2)
    // before entering the alternate screen; fall back to half-blocks.
    //
    // TWWH3_IMAGES=halfblocks or =off skips the query: if a terminal never
    // answers it, ratatui-image leaks a reader thread that steals
    // keystrokes from the TUI for the rest of the session.
    let picker = match setting("TWWH3_IMAGES", "images").as_deref() {
        Some("off") => None,
        Some("halfblocks") => Some(Picker::from_fontsize((8, 16))),
        _ => Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)))),
    };
    let mut app = App::load(moddata_path(), picker)?;

    // Service overlay mount requests from twwh3-run (which can't mount
    // itself when the game runs inside Steam's bwrap sandbox).
    start_mount_listener();

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    stop_mount_listener();
    // Don't leave a preview overlay mounted behind us.
    if app.preview_mounted {
        if let Some(game) = game_install_dir() {
            let _ = Command::new("fusermount3")
                .args(["-u"])
                .arg(game.join("data"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    res
}
