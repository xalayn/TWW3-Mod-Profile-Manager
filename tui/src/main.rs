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
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const APPID: u32 = 1142710;
const GAME: &str = "warhammer3";

// ---------------------------------------------------------------------------
// Paths

fn moddata_path() -> PathBuf {
    if let Ok(p) = env::var("TWWH3_MODDATA") {
        return PathBuf::from(p);
    }
    steam_root().join(format!(
        "steamapps/compatdata/{APPID}/pfx/drive_c/users/steamuser/\
         AppData/Roaming/The Creative Assembly/Launcher/20190104-moddata.dat"
    ))
}

fn steam_root() -> PathBuf {
    env::var("STEAM_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local/share/Steam")
        })
}

fn modlists_dir() -> PathBuf {
    if let Ok(p) = env::var("TWWH3_MODLISTS") {
        return PathBuf::from(p);
    }
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join("Games/TotalWarWH3/modlists")
}

fn workshop_dir() -> PathBuf {
    if let Ok(p) = env::var("TWWH3_WORKSHOP") {
        return PathBuf::from(p);
    }
    steam_root().join(format!("steamapps/workshop/content/{APPID}"))
}

/// Convert a wine path like "Z:/home/x/y.pack" to a unix path.
fn win_to_unix(p: &str) -> Option<PathBuf> {
    let p = p.replace('\\', "/");
    let rest = p.strip_prefix("Z:").or_else(|| p.strip_prefix("z:"))?;
    Some(PathBuf::from(rest))
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
        } else if app.slots.is_empty() {
            app.focus = Pane::Available;
        }
        Ok(app)
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
        let enabled = self.slots.iter().filter(|s| !self.slot_missing(s)).count();
        self.status = match &self.current_profile {
            Some(name) => match self.write_modlist(name) {
                Ok(()) => format!(
                    "Saved: {enabled} mods enabled + profile '{name}' (backup: {file_name}.bak)"
                ),
                Err(e) => format!("Saved mods, but updating profile '{name}' failed: {e:#}"),
            },
            None => format!("Saved: {enabled} mods enabled (backup: {file_name}.bak)"),
        };
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

    fn read_modlist(name: &str) -> Result<Vec<String>> {
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
            .filter_map(|e| e.get("uuid").and_then(Value::as_str))
            .map(String::from)
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
        for uuid in Self::read_modlist(&name).unwrap_or_default() {
            let known = self
                .pool
                .iter()
                .find(|m| m.uuid().is_some_and(|u| u.eq_ignore_ascii_case(&uuid)));
            self.preview.push(match known {
                Some(m) => PreviewLine {
                    label: m.name().to_string(),
                    missing: m.missing,
                },
                None => PreviewLine {
                    label: uuid,
                    missing: true,
                },
            });
        }
    }

    fn write_modlist(&self, name: &str) -> Result<()> {
        // Slots keep their uuid even when the mod is not installed, so
        // missing mods survive a rewrite.
        let mods: Vec<Value> = self
            .slots
            .iter()
            .map(|s| serde_json::json!({ "uuid": s.uuid, "active": true }))
            .collect();
        let dir = modlists_dir();
        fs::create_dir_all(&dir)?;
        let text = serde_json::to_string_pretty(&serde_json::json!({ "mods": mods }))?;
        fs::write(dir.join(format!("{name}.json")), text)?;
        Ok(())
    }

    fn save_profile(&mut self, name: &str) -> Result<()> {
        self.write_modlist(name)?;
        self.current_profile = Some(name.to_string());
        self.status = format!("Saved profile '{name}' ({} mods)", self.slots.len());
        Ok(())
    }

    fn apply_profile(&mut self, name: &str) -> Result<()> {
        let uuids = Self::read_modlist(name)?;
        self.slots = uuids
            .into_iter()
            .map(|uuid| {
                let idx = self
                    .pool
                    .iter()
                    .position(|m| m.uuid().is_some_and(|u| u.eq_ignore_ascii_case(&uuid)));
                Slot {
                    uuid: uuid.to_lowercase(),
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
        self.current_profile = Some(name.to_string());
        let note = if missing > 0 {
            format!(", {missing} missing (kept in profile, not enabled)")
        } else {
            String::new()
        };
        self.status = format!(
            "Applied profile '{name}': {} mods{note} — press s to save",
            self.slots.len()
        );
        Ok(())
    }

    fn delete_profile(&mut self, name: &str) -> Result<()> {
        fs::remove_file(modlists_dir().join(format!("{name}.json")))?;
        if self.current_profile.as_deref() == Some(name) {
            self.current_profile = None;
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

    let dirty_mark = if app.dirty { "  [modified]" } else { "" };
    let profile_mark = app
        .current_profile
        .as_ref()
        .map(|p| format!("  [profile: {p}]"))
        .unwrap_or_default();
    f.render_widget(
        Paragraph::new(format!(
            " twwh3-mods — {}{profile_mark}{dirty_mark}",
            app.path.display()
        ))
        .style(Style::default().add_modifier(Modifier::BOLD)),
        header,
    );

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

fn pane_block(title: String, focused: bool) -> Block<'static> {
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
            let name_style = if m.discovered {
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
            format!(" Available ({}) ", avail.len()),
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
                    (app.pool[i].name().to_string(), Style::default())
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
    let title = format!(
        " Load order — {} ({}) ",
        app.current_profile.as_deref().unwrap_or("unsaved"),
        app.slots.len()
    );
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
         Usage: twwh3-mods [--list]\n\n\
         Options:\n  \
           -l, --list   Print the load order and available mods, then exit\n  \
           -h, --help   Show this help\n\n\
         Keys:\n  \
           tab / h / l      switch pane   j/k or arrows        select\n  \
           space / enter    add to or remove from the load order\n  \
           J/K              reorder within the load order\n  \
           p                profiles (enter apply, n new, d delete)\n  \
           s                save          q                    quit\n\n\
         Environment:\n  \
           TWWH3_MODDATA   Path to the launcher moddata file (overrides everything)\n  \
           TWWH3_WORKSHOP  Workshop content dir to scan for new mods\n  \
           TWWH3_MODLISTS  Profile dir (default: ~/Games/TotalWarWH3/modlists)\n  \
           TWWH3_IMAGES    auto (default) | halfblocks | off\n  \
           STEAM_ROOT      Steam install root (default: ~/.local/share/Steam)"
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }
    if let Some(bad) = args.iter().find(|a| !matches!(a.as_str(), "-l" | "--list")) {
        usage();
        bail!("unknown argument: {bad}");
    }

    if !args.is_empty() {
        // --list: no terminal queries, no TUI.
        let app = App::load(moddata_path(), None)?;
        println!("Load order:");
        for (n, s) in app.slots.iter().enumerate() {
            let (name, note) = match s.idx {
                Some(i) if !app.pool[i].missing => (app.pool[i].name(), ""),
                Some(i) => (app.pool[i].name(), "  (missing)"),
                None => (s.uuid.as_str(), "  (missing)"),
            };
            println!("{:>3}  {name}{note}", n + 1);
        }
        println!("\nAvailable:");
        for i in app.available() {
            let m = &app.pool[i];
            let note = if m.discovered { "  (new)" } else { "" };
            println!("     {}{note}", m.name());
        }
        return Ok(());
    }

    // Query the terminal for its graphics protocol (kitty/sixel/iTerm2)
    // before entering the alternate screen; fall back to half-blocks.
    //
    // TWWH3_IMAGES=halfblocks or =off skips the query: if a terminal never
    // answers it, ratatui-image leaks a reader thread that steals
    // keystrokes from the TUI for the rest of the session.
    let picker = match env::var("TWWH3_IMAGES").as_deref() {
        Ok("off") => None,
        Ok("halfblocks") => Some(Picker::from_fontsize((8, 16))),
        _ => Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)))),
    };
    let mut app = App::load(moddata_path(), picker)?;

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    res
}
