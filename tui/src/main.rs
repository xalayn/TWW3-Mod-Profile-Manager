//! twwh3-mods — TUI load-order manager for Total War: WARHAMMER III.
//!
//! Edits the same file the official launcher uses
//! (Launcher/20190104-moddata.dat inside the game's Proton prefix), so
//! changes made here show up in the launcher and vice versa. Fields this
//! tool doesn't understand are preserved verbatim on save.
//!
//! Mod-list profiles (load order + enabled set, keyed by mod uuid) are
//! stored as JSON files in TWWH3_MODLISTS (default:
//! ~/Games/TotalWarWH3/modlists), independent of the full-folder snapshots
//! made by twwh3-profile.

use anyhow::{bail, Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use serde_json::Value;
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
    let steam_root = env::var("STEAM_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local/share/Steam")
        });
    steam_root.join(format!(
        "steamapps/compatdata/{APPID}/pfx/drive_c/users/steamuser/\
         AppData/Roaming/The Creative Assembly/Launcher/20190104-moddata.dat"
    ))
}

fn modlists_dir() -> PathBuf {
    if let Ok(p) = env::var("TWWH3_MODLISTS") {
        return PathBuf::from(p);
    }
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join("Games/TotalWarWH3/modlists")
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
        ModEntry {
            data,
            pack_path,
            size,
            steam_id,
            png,
            thumb: Thumb::NotLoaded,
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

    fn set_active(&mut self, on: bool) {
        if let Some(obj) = self.data.as_object_mut() {
            obj.insert("active".into(), Value::Bool(on));
        }
    }

    fn category(&self) -> &str {
        self.data
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("-")
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

struct App {
    path: PathBuf,
    mods: Vec<ModEntry>,
    /// Entries for other games in the same file, preserved untouched.
    others: Vec<Value>,
    table: TableState,
    picker: Option<Picker>,
    dirty: bool,
    status: String,
    confirm_quit: bool,
    mode: Mode,
    /// Profile that was last applied or saved; `s` keeps it in sync.
    current_profile: Option<String>,
    profiles: Vec<String>,
    profile_list: ListState,
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
        let mods: Vec<ModEntry> = wh3.into_iter().map(ModEntry::new).collect();
        let mut table = TableState::default();
        if !mods.is_empty() {
            table.select(Some(0));
        }
        Ok(App {
            path,
            mods,
            others,
            table,
            picker,
            dirty: false,
            status: String::new(),
            confirm_quit: false,
            mode: Mode::Browse,
            current_profile: None,
            profiles: Vec::new(),
            profile_list: ListState::default(),
            confirm_delete: false,
            input: String::new(),
        })
    }

    fn selected(&self) -> Option<usize> {
        self.table.selected().filter(|&i| i < self.mods.len())
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(i) = self.selected() else { return };
        let j = (i as isize + delta).clamp(0, self.mods.len() as isize - 1);
        self.table.select(Some(j as usize));
    }

    fn move_mod(&mut self, delta: isize) {
        let Some(i) = self.selected() else { return };
        let j = i as isize + delta;
        if j < 0 || j as usize >= self.mods.len() {
            return;
        }
        self.mods.swap(i, j as usize);
        self.table.select(Some(j as usize));
        self.dirty = true;
    }

    fn toggle(&mut self) {
        if let Some(i) = self.selected() {
            let on = self.mods[i].active();
            self.mods[i].set_active(!on);
            self.dirty = true;
        }
    }

    fn save(&mut self) -> Result<()> {
        let mut all: Vec<Value> = Vec::with_capacity(self.mods.len() + self.others.len());
        for (i, m) in self.mods.iter_mut().enumerate() {
            if let Some(obj) = m.data.as_object_mut() {
                obj.insert("order".into(), Value::from(i as i64 + 1));
            }
            all.push(m.data.clone());
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
        self.status = match &self.current_profile {
            Some(name) => match self.write_modlist(name) {
                Ok(()) => format!(
                    "Saved {} mods + profile '{name}' (backup: {file_name}.bak)",
                    self.mods.len()
                ),
                Err(e) => format!("Saved mods, but updating profile '{name}' failed: {e:#}"),
            },
            None => format!("Saved {} mods (backup: {file_name}.bak)", self.mods.len()),
        };
        Ok(())
    }

    // -- profiles ----------------------------------------------------------

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
    }

    fn selected_profile(&self) -> Option<&str> {
        self.profile_list
            .selected()
            .and_then(|i| self.profiles.get(i))
            .map(String::as_str)
    }

    fn write_modlist(&self, name: &str) -> Result<()> {
        let mods: Vec<Value> = self
            .mods
            .iter()
            .filter_map(|m| {
                let uuid = m.uuid()?;
                Some(serde_json::json!({ "uuid": uuid, "active": m.active() }))
            })
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
        self.status = format!("Saved profile '{name}' ({} mods)", self.mods.len());
        Ok(())
    }

    fn apply_profile(&mut self, name: &str) -> Result<()> {
        let path = modlists_dir().join(format!("{name}.json"));
        let text = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let root: Value = serde_json::from_str(&text)
            .with_context(|| format!("could not parse {}", path.display()))?;
        let entries = root
            .get("mods")
            .and_then(Value::as_array)
            .context("profile has no \"mods\" array")?;

        let mut wanted: Vec<(String, bool)> = Vec::new();
        for e in entries {
            if let Some(uuid) = e.get("uuid").and_then(Value::as_str) {
                let active = e.get("active").and_then(Value::as_bool).unwrap_or(true);
                wanted.push((uuid.to_string(), active));
            }
        }

        let pos = |m: &ModEntry| {
            m.uuid()
                .and_then(|u| wanted.iter().position(|(w, _)| w == u))
        };
        let mut matched = 0usize;
        for m in &mut self.mods {
            if let Some(p) = pos(m) {
                m.set_active(wanted[p].1);
                matched += 1;
            }
        }
        // Mods in the profile come first, in profile order; mods the profile
        // doesn't know about keep their relative order below them.
        self.mods
            .sort_by_key(|m| pos(m).unwrap_or(usize::MAX));
        let missing = wanted.len() - matched;
        let extra = self.mods.len() - matched;
        self.dirty = true;
        self.current_profile = Some(name.to_string());
        self.status = format!(
            "Applied profile '{name}': {matched} matched, {extra} not in profile, \
             {missing} in profile but not installed — press s to save"
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
    let [left, right] =
        Layout::horizontal([Constraint::Min(58), Constraint::Length(46)]).areas(main);

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

    draw_table(f, app, left);
    draw_side_panel(f, app, right);

    let help_line = if !app.status.is_empty() {
        Line::from(format!(" {}", app.status)).style(Style::default().fg(Color::Yellow))
    } else {
        let keys = match app.mode {
            Mode::Browse => {
                " j/k select · J/K reorder · space enable/disable · p profiles · s save · q quit"
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

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let header = Row::new(["#", "", "Name", "Category", "Size", "Steam ID"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .mods
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let style = if m.active() {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Row::new([
                format!("{:>3}", i + 1),
                (if m.active() { "[x]" } else { "[ ]" }).to_string(),
                m.name().to_string(),
                m.category().to_string(),
                format!("{:>9}", m.size.map(human_size).unwrap_or_else(|| "-".into())),
                m.steam_id.clone().unwrap_or_else(|| "-".into()),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(24),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Load order "))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ");
    f.render_stateful_widget(table, area, &mut app.table);
}

fn draw_side_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Mod ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(i) = app.selected() else {
        f.render_widget(Paragraph::new("no mods found for this game"), inner);
        return;
    };
    let [img_area, text_area] =
        Layout::vertical([Constraint::Length(12), Constraint::Min(1)]).areas(inner);

    // Load the thumbnail lazily, once per mod.
    let App { mods, picker, .. } = app;
    let entry = &mut mods[i];
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

    let entry = &app.mods[i];
    let label = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(Span::styled(entry.name().to_string(), Style::default().add_modifier(Modifier::BOLD))),
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
    let area = centered_rect(44, 16, f.area());
    f.render_widget(Clear, area);
    let items: Vec<ListItem> = if app.profiles.is_empty() {
        vec![ListItem::new("(no profiles yet — press n to create one)")
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
                .title_bottom(" enter apply · n new · d delete · esc close "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.profile_list);
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
                    KeyCode::Up if shift => app.move_mod(-1),
                    KeyCode::Down if shift => app.move_mod(1),
                    KeyCode::Char('K') => app.move_mod(-1),
                    KeyCode::Char('J') => app.move_mod(1),
                    KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                    KeyCode::Home | KeyCode::Char('g') => {
                        if !app.mods.is_empty() {
                            app.table.select(Some(0));
                        }
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        if let Some(last) = app.mods.len().checked_sub(1) {
                            app.table.select(Some(last));
                        }
                    }
                    KeyCode::Char(' ') => app.toggle(),
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
           -l, --list   Print the current load order and exit\n  \
           -h, --help   Show this help\n\n\
         Keys:\n  \
           j/k or arrows    select        J/K or shift+arrows  reorder\n  \
           space            enable/disable\n  \
           p                profiles (enter apply, n new, d delete)\n  \
           s                save          q                    quit\n\n\
         Environment:\n  \
           TWWH3_MODDATA   Path to the launcher moddata file (overrides everything)\n  \
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
        for (i, m) in app.mods.iter().enumerate() {
            let state = if m.active() { "on " } else { "off" };
            println!("{:>3}  {state}  {}", i + 1, m.name());
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
