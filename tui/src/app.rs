//! The TUI application: `App` state, its logic, and the event loop.

use crate::model::*;
use crate::paths::*;
use crate::steam::*;
use crate::ui::{draw, StatusLine};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use ratatui_image::picker::Picker;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// A profile name in `dir` that doesn't collide, appending " 2", " 3", …
/// to `base` if needed. Non-destructive: never overwrites an existing
/// profile.


#[derive(PartialEq)]
pub(crate) enum Mode {
    Browse,
    Profiles,
    NameInput,
    Status,
    Help,
    VersionPicker,
    RequirementPicker,
}

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum Pane {
    Available,
    Profile,
}

pub(crate) struct App {
    /// The launcher moddata file, read for Workshop names/seed (not written).
    pub(crate) path: PathBuf,
    /// Every known mod (Workshop + local folders). Never reordered.
    pub(crate) pool: Vec<ModEntry>,
    /// The load order: mods in here are enabled, in this order.
    pub(crate) slots: Vec<Slot>,
    pub(crate) focus: Pane,
    pub(crate) avail_state: TableState,
    pub(crate) prof_state: TableState,
    pub(crate) picker: Option<Picker>,
    pub(crate) dirty: bool,
    pub(crate) status: String,
    pub(crate) confirm_quit: bool,
    pub(crate) mode: Mode,
    /// Profile that was last applied or saved; `s` keeps it in sync.
    pub(crate) current_profile: Option<String>,
    /// Current workshop state: steam_id -> version info.
    pub(crate) ws: HashMap<String, WsInfo>,
    pub(crate) game_buildid: Option<String>,
    /// Version pins of the current profile: uuid -> pinned manifest.
    pub(crate) pins: HashMap<String, String>,
    pub(crate) pinned_buildid: Option<String>,
    pub(crate) profiles: Vec<String>,
    pub(crate) profile_list: ListState,
    /// Which profile the cached popup preview belongs to.
    pub(crate) preview_for: Option<String>,
    pub(crate) preview: Vec<PreviewLine>,
    pub(crate) confirm_delete: bool,
    pub(crate) input: String,
    /// When set, the name input renames this profile instead of creating.
    pub(crate) rename_from: Option<String>,
    /// Rows of the status page, computed when it is opened (S).
    pub(crate) status_lines: Vec<StatusLine>,
    /// We mounted a data/ overlay preview (o) and must unmount it.
    pub(crate) preview_mounted: bool,
    /// After L, watch twwh3-run's overlay-status file for a report newer
    /// than this launch epoch, to show which method the game launched with.
    pub(crate) overlay_watch_since: Option<u64>,
    /// The `v` version picker: pool index of the mod, its versions, and
    /// the highlighted row.
    pub(crate) version_mod: Option<usize>,
    pub(crate) versions: Vec<VersionInfo>,
    pub(crate) version_state: ListState,
    /// The requirement picker: which slot's requirements are being edited.
    pub(crate) req_slot: Option<usize>,
    pub(crate) req_state: ListState,
}

impl App {
    pub(crate) fn load(path: PathBuf, picker: Option<Picker>) -> Result<Self> {
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
            version_mod: None,
            versions: Vec::new(),
            version_state: ListState::default(),
            req_slot: None,
            req_state: ListState::default(),
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

    /// The pinned manifest differs from what's installed now.
    pub(crate) fn slot_updated(&self, s: &Slot) -> bool {
        let Some(pinned) = self.pins.get(&s.id) else { return false };
        let Some(i) = s.idx else { return false };
        let Some(sid) = &self.pool[i].steam_id else { return false };
        self.ws
            .get(sid)
            .is_some_and(|w| !w.manifest.is_empty() && &w.manifest != pinned)
    }

    /// Change the current profile and persist the choice.
    pub(crate) fn set_current(&mut self, name: Option<String>) {
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
    pub(crate) fn available(&self) -> Vec<usize> {
        let used: HashSet<usize> = self.slots.iter().filter_map(|s| s.idx).collect();
        let mut v: Vec<usize> = (0..self.pool.len())
            .filter(|i| !self.pool[*i].missing && !used.contains(i))
            .collect();
        v.sort_by_key(|&i| self.pool[i].name().to_lowercase());
        v
    }

    pub(crate) fn slot_missing(&self, s: &Slot) -> bool {
        s.idx.is_none_or(|i| self.pool[i].missing)
    }

    pub(crate) fn focused_len(&self) -> usize {
        match self.focus {
            Pane::Available => self.available().len(),
            Pane::Profile => self.slots.len(),
        }
    }

    pub(crate) fn focused_state(&mut self) -> &mut TableState {
        match self.focus {
            Pane::Available => &mut self.avail_state,
            Pane::Profile => &mut self.prof_state,
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let len = self.focused_len();
        let state = self.focused_state();
        let Some(i) = state.selected().filter(|&i| i < len) else { return };
        let j = (i as isize + delta).clamp(0, len as isize - 1);
        state.select(Some(j as usize));
    }

    pub(crate) fn select_edge(&mut self, end: bool) {
        let len = self.focused_len();
        let state = self.focused_state();
        if len > 0 {
            state.select(Some(if end { len - 1 } else { 0 }));
        }
    }

    pub(crate) fn switch_pane(&mut self) {
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

    /// Move the selected available mod into the load order.
    pub(crate) fn add_selected(&mut self) {
        let avail = self.available();
        let Some(sel) = self.avail_state.selected().filter(|&s| s < avail.len()) else {
            return;
        };
        let i = avail[sel];
        let id = self.pool[i].id().to_string();
        self.slots.push(Slot { id, idx: Some(i), enabled: true, requires: Vec::new() });
        self.dirty = true;
        let left = avail.len() - 1;
        self.avail_state
            .select(if left == 0 { None } else { Some(sel.min(left - 1)) });
        if self.prof_state.selected().is_none() {
            self.prof_state.select(Some(self.slots.len() - 1));
        }
    }

    /// Toggle the selected slot enabled/disabled — it keeps its spot in the
    /// load order but is skipped at launch when disabled.
    pub(crate) fn toggle_enabled(&mut self) {
        let Some(sel) = self.prof_state.selected().filter(|&s| s < self.slots.len()) else {
            return;
        };
        self.slots[sel].enabled = !self.slots[sel].enabled;
        self.dirty = true;
        let name = self.slot_name(sel);
        self.status = if self.slots[sel].enabled {
            format!("Enabled '{name}'")
        } else {
            format!("Disabled '{name}' (kept in the load order)")
        };
    }

    /// A slot's display name (mod name if installed, else its id).
    pub(crate) fn slot_name(&self, sel: usize) -> String {
        match self.slots[sel].idx {
            Some(i) => self.pool[i].name().to_string(),
            None => self.slots[sel].id.clone(),
        }
    }

    /// Display names of a slot's required mods that aren't satisfied —
    /// absent from the load order, or present but disabled/missing.
    pub(crate) fn unmet_requirements(&self, s: &Slot) -> Vec<String> {
        s.requires
            .iter()
            .filter(|req| {
                !self.slots.iter().any(|o| {
                    o.enabled
                        && o.id.eq_ignore_ascii_case(req)
                        && o.idx.is_some_and(|i| !self.pool[i].missing)
                })
            })
            .map(|req| {
                self.pool
                    .iter()
                    .find(|m| m.id().eq_ignore_ascii_case(req))
                    .map(|m| m.name().to_string())
                    .unwrap_or_else(|| req.clone())
            })
            .collect()
    }

    /// Slot indices eligible to be marked as requirements (every load-order
    /// mod except the one being edited).
    pub(crate) fn req_candidates(&self) -> Vec<usize> {
        match self.req_slot {
            Some(rs) => (0..self.slots.len()).filter(|&i| i != rs).collect(),
            None => Vec::new(),
        }
    }

    /// `R`: choose which other load-order mods the selected mod requires.
    pub(crate) fn open_requirement_picker(&mut self) {
        if self.focus != Pane::Profile {
            self.status = "Select a mod in the load order, then R to set what it requires".into();
            return;
        }
        let Some(sel) = self.prof_state.selected().filter(|&s| s < self.slots.len()) else {
            return;
        };
        if self.slots.len() < 2 {
            self.status = "Add more mods to the load order first".into();
            return;
        }
        self.req_slot = Some(sel);
        self.req_state.select(Some(0));
        self.mode = Mode::RequirementPicker;
    }

    /// Toggle the highlighted candidate as a requirement of the edited mod.
    pub(crate) fn toggle_requirement(&mut self) {
        let Some(rs) = self.req_slot else { return };
        let cands = self.req_candidates();
        let Some(&ci) = self.req_state.selected().and_then(|s| cands.get(s)) else {
            return;
        };
        let cand_id = self.slots[ci].id.clone();
        let reqs = &mut self.slots[rs].requires;
        if let Some(pos) = reqs.iter().position(|r| r.eq_ignore_ascii_case(&cand_id)) {
            reqs.remove(pos);
        } else {
            reqs.push(cand_id);
        }
        self.dirty = true;
    }

    /// Remove the selected slot from the load order.
    pub(crate) fn remove_selected(&mut self) {
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

    pub(crate) fn move_slot(&mut self, delta: isize) {
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

    /// The pool index of the mod highlighted in the focused pane.
    pub(crate) fn hovered_mod(&self) -> Option<usize> {
        match self.focus {
            Pane::Available => {
                let avail = self.available();
                self.avail_state
                    .selected()
                    .filter(|&s| s < avail.len())
                    .map(|s| avail[s])
            }
            Pane::Profile => self
                .prof_state
                .selected()
                .filter(|&s| s < self.slots.len())
                .and_then(|s| self.slots[s].idx),
        }
    }

}

pub(crate) fn valid_profile_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        && name != "."
        && name != ".."
}

pub(crate) fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
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
                        Pane::Profile => app.toggle_enabled(),
                    },
                    KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
                        if app.focus == Pane::Profile {
                            app.remove_selected();
                        }
                    }
                    KeyCode::Char('R') => app.open_requirement_picker(),
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
                    KeyCode::Char('v') => app.open_version_picker(),
                    KeyCode::Char('U') => app.update_all(),
                    KeyCode::Char('o') => app.toggle_data_view(),
                    KeyCode::Char('L') => app.launch(),
                    KeyCode::Char('?') => app.mode = Mode::Help,
                    _ => {}
                }
            }
            Mode::Help => {
                app.mode = Mode::Browse;
            }
            Mode::VersionPicker => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('v') => {
                    app.mode = Mode::Browse;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = app.version_state.selected().unwrap_or(0);
                    app.version_state.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !app.versions.is_empty() {
                        let i = app.version_state.selected().unwrap_or(0);
                        app.version_state.select(Some((i + 1).min(app.versions.len() - 1)));
                    }
                }
                KeyCode::Enter | KeyCode::Char(' ') => app.pick_version(),
                _ => {}
            },
            Mode::RequirementPicker => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('R') => {
                    app.req_slot = None;
                    app.mode = Mode::Browse;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = app.req_state.selected().unwrap_or(0);
                    app.req_state.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let n = app.req_candidates().len();
                    if n > 0 {
                        let i = app.req_state.selected().unwrap_or(0);
                        app.req_state.select(Some((i + 1).min(n - 1)));
                    }
                }
                KeyCode::Char(' ') => app.toggle_requirement(),
                _ => {}
            },
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
                KeyCode::Char('K') => app.move_profile(-1),
                KeyCode::Char('J') => app.move_profile(1),
                KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => app.move_profile(-1),
                KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => app.move_profile(1),
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
