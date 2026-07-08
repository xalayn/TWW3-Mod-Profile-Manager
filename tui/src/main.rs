//! twwh3-mods — TUI load-order manager for Total War: WARHAMMER III.
//!
//! Edits the same file the official launcher uses
//! (Launcher/20190104-moddata.dat inside the game's Proton prefix), so
//! changes made here show up in the launcher and vice versa. Fields this
//! tool doesn't understand are preserved verbatim on save.
//!
//! Two panes: "Available" lists every mod installed on disk (launcher
//! entries plus a scan of the Steam workshop content folder, so fresh
//! subscriptions show up immediately, marked "new"). The "Load order"
//! pane is the current profile: the ordered list of mods that will be
//! enabled; every mod not in it is saved as inactive. Profile entries
//! whose mod is not installed are shown as missing, skipped when
//! enabling, and preserved in the profile file.
//!
//! Mod-list profiles are stored as JSON files in TWWH3_MODLISTS
//! (default: ~/Games/TotalWarWH3/modlists), independent of the
//! full-folder snapshots made by twwh3-profile.

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
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Base dir for this tool's own data (profiles, vault, local mods).
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

/// Drop .pack files here to use mods from outside the Steam Workshop.
fn local_mods_dir() -> PathBuf {
    path_setting("TWWH3_LOCAL", "local_mods").unwrap_or_else(|| data_dir().join("mods"))
}

/// Local store of past mod versions: vault/<steam_id>/<manifest>/<pack>.
fn vault_dir() -> PathBuf {
    path_setting("TWWH3_VAULT", "vault").unwrap_or_else(|| data_dir().join("vault"))
}

/// Convert a wine path like "Z:/home/x/y.pack" to a unix path.
fn win_to_unix(p: &str) -> Option<PathBuf> {
    let p = p.replace('\\', "/");
    let rest = p.strip_prefix("Z:").or_else(|| p.strip_prefix("z:"))?;
    Some(PathBuf::from(rest))
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
                        // Both ItemsInstalled and ItemDetails carry these
                        // fields; prefer whichever copy has a manifest.
                        match map.get(&id) {
                            Some(old) if !old.manifest.is_empty() && info.manifest.is_empty() => {}
                            _ => {
                                map.insert(id, info);
                            }
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

struct ModEntry {
    data: Value,
    pack_path: Option<PathBuf>,
    size: Option<u64>,
    steam_id: Option<String>,
    png: Option<PathBuf>,
    thumb: Thumb,
    /// Found on disk but not yet known to the launcher.
    discovered: bool,
    /// From the local mods dir rather than the Steam Workshop.
    local: bool,
    /// Pack file no longer exists on disk (unsubscribed / not downloaded).
    missing: bool,
}

impl ModEntry {
    fn new(data: Value) -> Self {
        let pack_path = data
            .get("packfile")
            .and_then(Value::as_str)
            .and_then(win_to_unix);
        let (size, steam_id, png) = match &pack_path {
            Some(p) => {
                let size = fs::metadata(p).ok().map(|m| m.len());
                // Workshop packs live in .../workshop/content/<appid>/<steam_id>/
                let steam_id = p
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                    .map(String::from);
                let png = Some(p.with_extension("png"))
                    .filter(|c| c.exists())
                    .or_else(|| {
                        fs::read_dir(p.parent()?).ok()?.flatten().map(|e| e.path()).find(
                            |q| q.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")),
                        )
                    });
                (size, steam_id, png)
            }
            None => (None, None, None),
        };
        let missing = pack_path.as_ref().is_none_or(|p| !p.exists());
        ModEntry {
            data,
            pack_path,
            size,
            steam_id,
            png,
            thumb: Thumb::NotLoaded,
            discovered: false,
            local: false,
            missing,
        }
    }

    fn name(&self) -> &str {
        self.data
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| self.uuid())
            .unwrap_or("<unnamed>")
    }

    fn uuid(&self) -> Option<&str> {
        self.data.get("uuid").and_then(Value::as_str)
    }

    fn active(&self) -> bool {
        self.data.get("active").and_then(Value::as_bool).unwrap_or(false)
    }

    fn set(&mut self, key: &str, value: Value) {
        if let Some(obj) = self.data.as_object_mut() {
            obj.insert(key.into(), value);
        }
    }

    fn category(&self) -> &str {
        match self.data.get("category").and_then(Value::as_str) {
            Some("") | None => "-",
            Some(c) => c,
        }
    }

    /// Workshop description snippet with BBCode-style [tags] stripped.
    fn description(&self) -> String {
        let raw = self
            .data
            .get("short")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut out = String::with_capacity(raw.len());
        let mut in_tag = false;
        for c in raw.chars() {
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

/// Scan the Steam workshop content folder for pack files the launcher
/// doesn't know about yet and append them as entries.
fn discover_workshop_mods(mods: &mut Vec<ModEntry>) {
    let known: HashSet<String> = mods
        .iter()
        .filter_map(|m| m.uuid())
        .map(str::to_lowercase)
        .collect();
    let Ok(items) = fs::read_dir(workshop_dir()) else { return };
    let mut found: Vec<ModEntry> = Vec::new();
    for item in items.flatten() {
        let Ok(files) = fs::read_dir(item.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")) {
                continue;
            }
            let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
            // The launcher uses the lowercased pack file name as the uuid.
            let uuid = fname.to_lowercase();
            if known.contains(&uuid) {
                continue;
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(fname);
            let data = serde_json::json!({
                "uuid": uuid,
                "order": 0,
                "active": false,
                "game": GAME,
                "packfile": format!("Z:{}", p.display()),
                "name": stem,
                "short": "",
                "category": "",
                "owned": false,
            });
            let mut e = ModEntry::new(data);
            e.discovered = true;
            found.push(e);
        }
    }
    found.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    mods.extend(found);
}

/// Scan the local mods dir for .pack files (mods from outside the
/// Steam Workshop) and append them as entries.
fn discover_local_mods(mods: &mut Vec<ModEntry>) {
    let dir = local_mods_dir();
    let _ = fs::create_dir_all(&dir);
    let known: HashSet<String> = mods
        .iter()
        .filter_map(|m| m.uuid())
        .map(str::to_lowercase)
        .collect();
    let Ok(files) = fs::read_dir(&dir) else { return };
    let mut found: Vec<ModEntry> = Vec::new();
    for f in files.flatten() {
        let p = f.path();
        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")) {
            continue;
        }
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
        let uuid = fname.to_lowercase();
        if known.contains(&uuid) {
            continue;
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(fname);
        let data = serde_json::json!({
            "uuid": uuid,
            "order": 0,
            "active": false,
            "game": GAME,
            "packfile": format!("Z:{}", p.display()),
            "name": stem,
            "short": "",
            "category": "",
            "owned": true,
        });
        let mut e = ModEntry::new(data);
        e.discovered = true;
        e.local = true;
        found.push(e);
    }
    found.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    mods.extend(found);
}

/// Copy a mod's pack (+ thumbnail) into the vault under its manifest id,
/// unless that exact version is already vaulted. Returns true if copied.
fn vault_pack(entry: &ModEntry, manifest: &str) -> Result<bool> {
    let (Some(sid), Some(pack)) = (&entry.steam_id, &entry.pack_path) else {
        return Ok(false);
    };
    if manifest.is_empty() || entry.missing {
        return Ok(false);
    }
    let dst_dir = vault_dir().join(sid).join(manifest);
    let Some(fname) = pack.file_name() else { return Ok(false) };
    let dst = dst_dir.join(fname);
    if dst.exists() {
        return Ok(false);
    }
    fs::create_dir_all(&dst_dir)
        .with_context(|| format!("could not create {}", dst_dir.display()))?;
    // Copy to a temp name first so an interrupted copy can't be mistaken
    // for a complete vaulted version.
    let tmp = dst_dir.join(format!(".{}.tmp", fname.to_string_lossy()));
    fs::copy(pack, &tmp).with_context(|| format!("could not vault {}", pack.display()))?;
    fs::rename(&tmp, &dst)?;
    if let Some(png) = &entry.png {
        if let Some(png_name) = png.file_name() {
            let _ = fs::copy(png, dst_dir.join(png_name));
        }
    }
    Ok(true)
}

/// Where the vaulted copy of a specific mod version lives, if it exists.
fn vaulted_pack(entry: &ModEntry, manifest: &str) -> Option<PathBuf> {
    let sid = entry.steam_id.as_ref()?;
    let fname = entry.pack_path.as_ref()?.file_name()?;
    let p = vault_dir().join(sid).join(manifest).join(fname);
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
// App state

#[derive(PartialEq)]
enum Mode {
    Browse,
    Profiles,
    NameInput,
}

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Available,
    Profile,
}

/// One position in the load order: a mod uuid, resolved to the pool
/// where possible. `idx: None` means the mod is not installed at all.
struct Slot {
    uuid: String,
    idx: Option<usize>,
}

struct PreviewLine {
    label: String,
    missing: bool,
    updated: bool,
}

/// A profile entry as stored on disk (v2 adds version pins).
struct MlEntry {
    uuid: String,
    steam_id: Option<String>,
    manifest: Option<String>,
}

struct App {
    path: PathBuf,
    /// Every known mod: launcher entries + workshop scan. Never reordered.
    pool: Vec<ModEntry>,
    /// Entries for other games in the same file, preserved untouched.
    others: Vec<Value>,
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
}

impl App {
    fn load(path: PathBuf, picker: Option<Picker>) -> Result<Self> {
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "could not read launcher mod data at {}\n\
                 Has the game's launcher been run at least once?\n\
                 Set TWWH3_MODDATA or STEAM_ROOT if your Steam library lives elsewhere.",
                path.display()
            )
        })?;
        let root: Value = serde_json::from_str(&text)
            .with_context(|| format!("could not parse {} as JSON", path.display()))?;
        let Value::Array(entries) = root else {
            bail!("{}: expected a JSON array", path.display());
        };
        let (mut wh3, others): (Vec<Value>, Vec<Value>) = entries
            .into_iter()
            .partition(|m| m.get("game").and_then(Value::as_str) == Some(GAME));
        wh3.sort_by_key(|m| m.get("order").and_then(Value::as_i64).unwrap_or(i64::MAX));
        let mut pool: Vec<ModEntry> = wh3.into_iter().map(ModEntry::new).collect();
        discover_workshop_mods(&mut pool);
        discover_local_mods(&mut pool);
        // Entries the launcher already adopted from the local dir.
        for m in &mut pool {
            if !m.local {
                m.local = m
                    .pack_path
                    .as_ref()
                    .is_some_and(|p| p.starts_with(local_mods_dir()));
            }
        }

        // Initial load order: the launcher's active mods, in its order.
        let slots: Vec<Slot> = pool
            .iter()
            .enumerate()
            .filter(|(_, m)| m.active() && m.uuid().is_some())
            .map(|(i, m)| Slot {
                uuid: m.uuid().unwrap_or_default().to_lowercase(),
                idx: Some(i),
            })
            .collect();

        let mut app = App {
            path,
            pool,
            others,
            slots,
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
        };
        if !app.slots.is_empty() {
            app.prof_state.select(Some(0));
        }
        if !app.available().is_empty() {
            app.avail_state.select(Some(0));
        }
        if app.slots.is_empty() {
            app.focus = Pane::Available;
        }
        // Restore which profile we were on, if it still exists.
        app.current_profile = fs::read_to_string(current_profile_file())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|n| !n.is_empty())
            .filter(|n| modlists_dir().join(format!("{n}.json")).exists());
        if let Some(name) = app.current_profile.clone() {
            app.load_pins(&name);
            app.status = app.drift_report().unwrap_or_default();
        }
        Ok(app)
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
            if let (Some(uuid), Some(man)) = (
                e.get("uuid").and_then(Value::as_str),
                e.get("manifest").and_then(Value::as_str),
            ) {
                self.pins.insert(uuid.to_lowercase(), man.to_string());
            }
        }
    }

    /// The pinned manifest differs from what's installed now.
    fn slot_updated(&self, s: &Slot) -> bool {
        let Some(pinned) = self.pins.get(&s.uuid) else { return false };
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
            "Since '{name}' was saved: {} — L launches with pinned versions where vaulted",
            parts.join("; ")
        ))
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
        let Some(uuid) = self.pool[i].uuid().map(str::to_lowercase) else { return };
        self.slots.push(Slot { uuid, idx: Some(i) });
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

    fn save(&mut self) -> Result<()> {
        let backup = self.save_moddata()?;
        let enabled = self.slots.iter().filter(|s| !self.slot_missing(s)).count();
        self.status = match self.current_profile.clone() {
            Some(name) => match self.write_modlist(&name) {
                Ok(vaulted) if vaulted > 0 => format!(
                    "Saved: {enabled} mods enabled + profile '{name}', {vaulted} pack versions vaulted (backup: {backup})"
                ),
                Ok(_) => format!(
                    "Saved: {enabled} mods enabled + profile '{name}' (backup: {backup})"
                ),
                Err(e) => format!("Saved mods, but updating profile '{name}' failed: {e:#}"),
            },
            None => format!("Saved: {enabled} mods enabled (backup: {backup})"),
        };
        Ok(())
    }

    /// Write the launcher moddata file only. Returns the backup file name.
    fn save_moddata(&mut self) -> Result<String> {
        // Mods in the load order become active (unless their pack is
        // gone) and come first; everything else is inactive, after them.
        let in_order: Vec<usize> = self.slots.iter().filter_map(|s| s.idx).collect();
        let used: HashSet<usize> = in_order.iter().copied().collect();
        let mut sequence = in_order;
        sequence.extend((0..self.pool.len()).filter(|i| !used.contains(i)));

        for i in 0..self.pool.len() {
            self.pool[i].set("active", Value::Bool(false));
        }
        for s in &self.slots {
            if let Some(i) = s.idx {
                if !self.pool[i].missing {
                    self.pool[i].set("active", Value::Bool(true));
                }
            }
        }
        let mut all: Vec<Value> = Vec::with_capacity(self.pool.len() + self.others.len());
        for (n, &i) in sequence.iter().enumerate() {
            self.pool[i].set("order", Value::from(n as i64 + 1));
            all.push(self.pool[i].data.clone());
        }
        all.extend(self.others.iter().cloned());
        let text = serde_json::to_string(&Value::Array(all))?;

        let file_name = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("moddata.dat")
            .to_string();
        if self.path.exists() {
            let backup = self.path.with_file_name(format!("{file_name}.bak"));
            fs::copy(&self.path, &backup).context("could not create backup")?;
        }
        // Write to a temp file and rename so a failed write can't truncate
        // the launcher's data.
        let tmp = self.path.with_file_name(format!("{file_name}.tmp"));
        fs::write(&tmp, &text)?;
        fs::rename(&tmp, &self.path)?;

        self.dirty = false;
        Ok(format!("{file_name}.bak"))
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
                Some(MlEntry {
                    uuid: e.get("uuid").and_then(Value::as_str)?.to_string(),
                    steam_id: e
                        .get("steam_id")
                        .and_then(Value::as_str)
                        .map(String::from),
                    manifest: e
                        .get("manifest")
                        .and_then(Value::as_str)
                        .map(String::from),
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
            let known = self
                .pool
                .iter()
                .find(|m| m.uuid().is_some_and(|u| u.eq_ignore_ascii_case(&e.uuid)));
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
                    label: e.uuid,
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
        // Slots keep their uuid even when the mod is not installed, so
        // missing mods survive a rewrite.
        let mut vaulted = 0usize;
        let mut mods: Vec<Value> = Vec::with_capacity(self.slots.len());
        for s in &self.slots {
            let mut o = serde_json::json!({ "uuid": s.uuid, "active": true });
            if let Some(entry) = s.idx.map(|i| &self.pool[i]) {
                if let Some(sid) = &entry.steam_id {
                    o["steam_id"] = Value::from(sid.clone());
                    if let Some(w) = self.ws.get(sid) {
                        o["manifest"] = Value::from(w.manifest.clone());
                        o["timeupdated"] = Value::from(w.timeupdated);
                        o["size"] = Value::from(w.size);
                        if vault_pack(entry, &w.manifest)? {
                            vaulted += 1;
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
        let entries = Self::read_modlist(name)?;
        self.slots = entries
            .into_iter()
            .map(|e| {
                let idx = self
                    .pool
                    .iter()
                    .position(|m| m.uuid().is_some_and(|u| u.eq_ignore_ascii_case(&e.uuid)));
                Slot {
                    uuid: e.uuid.to_lowercase(),
                    idx,
                }
            })
            .collect();
        let missing = self.slots.iter().filter(|s| self.slot_missing(s)).count();
        self.prof_state
            .select(if self.slots.is_empty() { None } else { Some(0) });
        if self.avail_state.selected().is_none() && !self.available().is_empty() {
            self.avail_state.select(Some(0));
        }
        self.dirty = true;
        self.set_current(Some(name.to_string()));
        self.load_pins(name);
        // Switching profiles takes effect immediately — but only the
        // launcher file is written; the profile file (and its version
        // pins) is left untouched so drift stays visible.
        self.save_moddata()?;
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

    /// Generate used_mods.txt in the game dir from the load order. Mods
    /// whose workshop copy was updated past the profile's pin use the
    /// vaulted (pinned) version when it exists. Returns (mods, pinned).
    fn write_used_mods(&self) -> Result<(usize, usize)> {
        let game_dir = game_install_dir().context(
            "could not find the game install dir (steamapps/common/Total War WARHAMMER III)",
        )?;
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut mod_lines: Vec<String> = Vec::new();
        let mut pinned = 0usize;
        for s in &self.slots {
            let Some(entry) = s.idx.map(|i| &self.pool[i]) else { continue };
            if entry.missing {
                continue;
            }
            let Some(live) = &entry.pack_path else { continue };
            let path = if self.slot_updated(s) {
                match self.pins.get(&s.uuid).and_then(|m| vaulted_pack(entry, m)) {
                    Some(vaulted) => {
                        pinned += 1;
                        vaulted
                    }
                    None => live.clone(),
                }
            } else {
                live.clone()
            };
            let Some(dir) = path.parent() else { continue };
            let Some(fname) = path.file_name().and_then(|f| f.to_str()) else { continue };
            if !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_path_buf());
            }
            mod_lines.push(format!("mod \"{fname}\";"));
        }
        let mut out = String::new();
        for d in &dirs {
            out.push_str(&format!("add_working_directory \"{}\";\n", unix_to_win(d)));
        }
        for l in &mod_lines {
            out.push_str(l);
            out.push('\n');
        }
        fs::write(game_dir.join("used_mods.txt"), out)
            .with_context(|| format!("could not write used_mods.txt in {}", game_dir.display()))?;
        Ok((mod_lines.len(), pinned))
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
        match Command::new("steam")
            .args(["-applaunch", &APPID.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => {
                let pin_note = if pinned > 0 {
                    format!(", {pinned} pinned from vault")
                } else {
                    String::new()
                };
                self.status = format!(
                    "Launching via Steam: {mods} mods in used_mods.txt{pin_note} (see --help for the one-time launch options setup)"
                );
            }
            Err(e) => self.status = format!("could not run steam: {e}"),
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
        let keys = match app.mode {
            Mode::Browse => {
                " tab pane · j/k select · space add/remove · J/K reorder · p profiles · s save · q quit"
            }
            Mode::Profiles => " enter apply · n new · d delete · esc close",
            Mode::NameInput => " enter save · esc cancel",
        };
        Line::from(keys).style(Style::default().fg(Color::DarkGray))
    };
    f.render_widget(Paragraph::new(help_line), help);

    match app.mode {
        Mode::Profiles => draw_profiles_popup(f, app),
        Mode::NameInput => draw_name_input(f, app),
        Mode::Browse => {}
    }
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
            } else if m.discovered {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(m.name().to_string()).style(name_style),
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
                    format!("{} (missing)", s.uuid),
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
                None => SidePanelSubject::Unknown(slot.uuid.clone()),
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
            Span::styled("category: ", label),
            Span::raw(entry.category().to_string()),
        ]),
        Line::from(vec![
            Span::styled("steam id: ", label),
            Span::raw(entry.steam_id.clone().unwrap_or_else(|| "-".into())),
        ]),
        Line::from(vec![
            Span::styled("pack: ", label),
            Span::raw(
                entry
                    .pack_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string(),
            ),
        ]),
        Line::from(""),
    ];
    if entry.missing {
        lines.insert(
            1,
            Line::from("missing — pack file not found (unsubscribed or not downloaded)")
                .style(Style::default().fg(Color::Red)),
        );
    } else if entry.local {
        lines.insert(
            1,
            Line::from("local mod — loaded from the local mods dir, not Steam Workshop")
                .style(Style::default().fg(Color::Magenta)),
        );
    } else if entry.discovered {
        lines.insert(
            1,
            Line::from("new — not in the launcher list yet; added when you save")
                .style(Style::default().fg(Color::Green)),
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
                .title_bottom(" n new · d delete "),
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
    f.render_widget(
        Paragraph::new(format!("{}▏", app.input)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" New profile name "),
        ),
        area,
    );
}

// ---------------------------------------------------------------------------
// Event loop

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
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
                    KeyCode::Char('L') => app.launch(),
                    _ => {}
                }
            }
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
                    app.mode = Mode::NameInput;
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
                KeyCode::Esc => app.mode = Mode::Profiles,
                KeyCode::Enter => {
                    let name = app.input.trim().to_string();
                    if !valid_profile_name(&name) {
                        app.status =
                            "Profile names: letters, digits, space, - _ . only".into();
                    } else {
                        if let Err(e) = app.save_profile(&name) {
                            app.status = format!("Could not save profile: {e:#}");
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
         Usage: twwh3-mods [--list | --launch | --paths]\n\n\
         Options:\n  \
           -l, --list   Print the load order and available mods, then exit\n  \
           --launch     Write used_mods.txt and start the game via Steam\n  \
           --paths      Print every resolved path and where config is read from\n  \
           -h, --help   Show this help\n\n\
         Keys:\n  \
           tab / h / l      switch pane   j/k or arrows        select\n  \
           space / enter    add to or remove from the load order\n  \
           J/K              reorder within the load order\n  \
           p                profiles (enter apply, n new, d delete)\n  \
           s                save          L launch          q  quit\n\n\
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
         Local (non-Workshop) mods:\n  \
           Drop .pack files into ~/Games/TotalWarWH3/mods (TWWH3_LOCAL) and\n  \
           they appear under Available, marked 'local'. They load from that\n  \
           folder directly — no need to copy them into the game's data dir.\n\n\
         Versioning:\n  \
           Profiles pin each mod's Steam manifest (= exact version) and the\n  \
           game build id at save time; the packs themselves are copied into\n  \
           the vault. When Steam force-updates a mod, the load order marks\n  \
           it '(updated)' and L loads the pinned version from the vault.\n\n\
         Configuration (~/.config/twwh3-mods/config, `key = value` lines):\n  \
           steam_root  Steam library containing the game (default: ~/.local/share/Steam)\n  \
           data_dir    Base for this tool's data (default: ~/Games/TotalWarWH3)\n  \
           modlists    Profiles          (default: <data_dir>/modlists)\n  \
           vault       Pinned versions   (default: <data_dir>/vault)\n  \
           local_mods  Non-Workshop mods (default: <data_dir>/mods)\n  \
           moddata     Launcher mod list file    (default: derived from steam_root)\n  \
           workshop    Workshop content dir      (default: derived from steam_root)\n  \
           game_dir    Game install dir          (default: derived from steam_root)\n  \
           images      auto (default) | halfblocks | off\n\n  \
           Each key also has an env var that overrides it: STEAM_ROOT,\n  \
           TWWH3_DATA, TWWH3_MODLISTS, TWWH3_VAULT, TWWH3_LOCAL,\n  \
           TWWH3_MODDATA, TWWH3_WORKSHOP, TWWH3_GAME, TWWH3_IMAGES.\n  \
           Run `twwh3-mods --paths` to see the resolved values."
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
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
        println!("vault:        {}", vault_dir().display());
        println!("local_mods:   {}", local_mods_dir().display());
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
                None => (s.uuid.as_str(), "  (missing)"),
            };
            out.push_str(&format!("{:>3}  {name}{note}\n", n + 1));
        }
        out.push_str("\nAvailable:\n");
        for i in app.available() {
            let m = &app.pool[i];
            let note = if m.local {
                "  (local)"
            } else if m.discovered {
                "  (new)"
            } else {
                ""
            };
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

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    res
}
