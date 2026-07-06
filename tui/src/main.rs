//! twwh3-mods — TUI load-order manager for Total War: WARHAMMER III.
//!
//! Edits the same file the official launcher uses
//! (Launcher/20190104-moddata.dat inside the game's Proton prefix), so
//! changes made here show up in the launcher and vice versa. Fields this
//! tool doesn't understand are preserved verbatim on save.

use anyhow::{bail, Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;

const APPID: u32 = 1142710;
const GAME: &str = "warhammer3";

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

fn mod_name(m: &Value) -> &str {
    m.get("name")
        .and_then(Value::as_str)
        .or_else(|| m.get("uuid").and_then(Value::as_str))
        .unwrap_or("<unnamed>")
}

fn is_active(m: &Value) -> bool {
    m.get("active").and_then(Value::as_bool).unwrap_or(false)
}

struct App {
    path: PathBuf,
    /// WH3 mod entries, in display (= load) order.
    mods: Vec<Value>,
    /// Entries for other games in the same file, preserved untouched.
    others: Vec<Value>,
    list: ListState,
    dirty: bool,
    status: String,
    confirm_quit: bool,
}

impl App {
    fn load(path: PathBuf) -> Result<Self> {
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
        let (mut mods, others): (Vec<Value>, Vec<Value>) = entries
            .into_iter()
            .partition(|m| m.get("game").and_then(Value::as_str) == Some(GAME));
        mods.sort_by_key(|m| m.get("order").and_then(Value::as_i64).unwrap_or(i64::MAX));
        let mut list = ListState::default();
        if !mods.is_empty() {
            list.select(Some(0));
        }
        Ok(App {
            path,
            mods,
            others,
            list,
            dirty: false,
            status: String::new(),
            confirm_quit: false,
        })
    }

    fn selected(&self) -> Option<usize> {
        self.list.selected().filter(|&i| i < self.mods.len())
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(i) = self.selected() else { return };
        let j = (i as isize + delta).clamp(0, self.mods.len() as isize - 1);
        self.list.select(Some(j as usize));
    }

    fn move_mod(&mut self, delta: isize) {
        let Some(i) = self.selected() else { return };
        let j = i as isize + delta;
        if j < 0 || j as usize >= self.mods.len() {
            return;
        }
        self.mods.swap(i, j as usize);
        self.list.select(Some(j as usize));
        self.dirty = true;
    }

    fn toggle(&mut self) {
        let Some(i) = self.selected() else { return };
        if let Some(obj) = self.mods[i].as_object_mut() {
            let cur = obj.get("active").and_then(Value::as_bool).unwrap_or(false);
            obj.insert("active".into(), Value::Bool(!cur));
            self.dirty = true;
        }
    }

    fn save(&mut self) -> Result<()> {
        for (i, m) in self.mods.iter_mut().enumerate() {
            if let Some(obj) = m.as_object_mut() {
                obj.insert("order".into(), Value::from(i as i64 + 1));
            }
        }
        let mut all = self.mods.clone();
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
        self.status = format!("Saved {} mods (backup: {file_name}.bak)", self.mods.len());
        Ok(())
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, detail, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let dirty_mark = if app.dirty { "  [modified]" } else { "" };
    f.render_widget(
        Paragraph::new(format!(" twwh3-mods — {}{dirty_mark}", app.path.display()))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        header,
    );

    let items: Vec<ListItem> = app
        .mods
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let style = if is_active(m) {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let marker = if is_active(m) { "[x]" } else { "[ ]" };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>3} ", i + 1), Style::default().fg(Color::Yellow)),
                Span::styled(format!("{marker} {}", mod_name(m)), style),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Load order "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, body, &mut app.list);

    let detail_lines = match app.selected().map(|i| &app.mods[i]) {
        Some(m) => {
            let pack = m.get("packfile").and_then(Value::as_str).unwrap_or("?");
            let cat = m.get("category").and_then(Value::as_str).unwrap_or("?");
            vec![
                Line::from(format!(" pack: {pack}")),
                Line::from(format!(" category: {cat}")),
            ]
        }
        None => vec![Line::from(" no mods found for this game")],
    };
    f.render_widget(
        Paragraph::new(detail_lines).style(Style::default().fg(Color::DarkGray)),
        detail,
    );

    let help_line = if app.status.is_empty() {
        Line::from(" ↑/↓ or j/k select · shift+↑/↓ or J/K reorder · space enable/disable · s save · q quit")
            .style(Style::default().fg(Color::DarkGray))
    } else {
        Line::from(format!(" {}", app.status)).style(Style::default().fg(Color::Yellow))
    };
    f.render_widget(Paragraph::new(help_line), help);
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let was_confirm = app.confirm_quit;
        app.confirm_quit = false;
        app.status.clear();
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char('q') | KeyCode::Esc => {
                if !app.dirty || was_confirm {
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
                    app.list.select(Some(0));
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if let Some(last) = app.mods.len().checked_sub(1) {
                    app.list.select(Some(last));
                }
            }
            KeyCode::Char(' ') => app.toggle(),
            KeyCode::Char('s') => {
                if let Err(e) = app.save() {
                    app.status = format!("Save failed: {e:#}");
                }
            }
            _ => {}
        }
    }
}

fn usage() {
    println!(
        "twwh3-mods — TUI mod load-order manager for Total War: WARHAMMER III\n\n\
         Usage: twwh3-mods [--list]\n\n\
         Options:\n  \
           -l, --list   Print the current load order and exit\n  \
           -h, --help   Show this help\n\n\
         Environment:\n  \
           TWWH3_MODDATA  Path to the launcher moddata file (overrides everything)\n  \
           STEAM_ROOT     Steam install root (default: ~/.local/share/Steam)"
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

    let mut app = App::load(moddata_path())?;

    if !args.is_empty() {
        for (i, m) in app.mods.iter().enumerate() {
            let state = if is_active(m) { "on " } else { "off" };
            println!("{:>3}  {state}  {}", i + 1, mod_name(m));
        }
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    res
}
