//! Rendering: every `draw_*` widget and the status-page row type.

use crate::app::{App, Mode, Pane};
use crate::model::*;
use crate::paths::tilde;
use crate::store::{fmt_date, human_size};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap,
};
use ratatui::Frame;
use ratatui_image::StatefulImage;
use std::env;
use std::path::Path;

/// One row of the status page. `ok` drives the colour: true = green,
/// false = red, None = informational. An empty value marks a section
/// header.
pub(crate) struct StatusLine {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) ok: Option<bool>,
}

impl StatusLine {
    pub(crate) fn section(label: &str) -> Self {
        Self { label: label.into(), value: String::new(), ok: None }
    }
    pub(crate) fn new(label: &str, value: impl Into<String>, ok: Option<bool>) -> Self {
        Self { label: label.into(), value: value.into(), ok }
    }
}

/// A path row: green when it exists, `missing_note` (with `missing_ok`
/// severity) when it doesn't.
pub(crate) fn path_line(label: &str, p: &Path, missing_note: &str, missing_ok: Option<bool>) -> StatusLine {
    if p.exists() {
        StatusLine::new(label, tilde(p), Some(true))
    } else {
        StatusLine::new(label, format!("{} {}", tilde(p), missing_note), missing_ok)
    }
}

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
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
            Mode::Profiles => " enter apply · J/K reorder · n new · r rename · e export · d delete · esc",
            Mode::NameInput => " enter confirm · esc cancel",
            Mode::VersionPicker => " enter pin · j/k choose · esc cancel",
            Mode::RequirementPicker => " space toggle · j/k choose · esc done",
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
        Mode::VersionPicker => draw_version_picker(f, app),
        Mode::RequirementPicker => draw_requirement_picker(f, app),
        Mode::Browse => {}
    }
}

/// All keybindings, grouped, shown by `?`.
pub(crate) fn draw_help_popup(f: &mut Frame) {
    // (key, description); empty key = section header, empty both = spacer.
    let rows: &[(&str, &str)] = &[
        ("Navigation", ""),
        ("tab / h / l", "switch pane"),
        ("j / k / ↑ / ↓", "move selection"),
        ("g / G", "jump to top / bottom"),
        ("", ""),
        ("Load order", ""),
        ("space / enter", "Available: add · Load order: enable/disable in place"),
        ("x / del", "remove the selected mod from the load order"),
        ("J / K", "reorder the selected mod"),
        ("R", "set which other mods this one requires"),
        ("v", "pick which stored version of a Workshop mod to use"),
        ("U", "update all mods flagged (updated) to current"),
        ("", ""),
        ("Profiles (p)", ""),
        ("enter", "apply the selected profile"),
        ("J / K / shift+↑↓", "reorder profiles"),
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

/// The `v` version picker: the Workshop mod's stored versions + the one
/// Steam has installed now, newest first, with update dates.
pub(crate) fn draw_version_picker(f: &mut Frame, app: &mut App) {
    let name = app.version_mod.map(|i| app.pool[i].name().to_string()).unwrap_or_default();
    let id = app.version_mod.map(|i| app.pool[i].id().to_string()).unwrap_or_default();
    let pinned = app.pins.get(&id).cloned();
    let items: Vec<ListItem> = app
        .versions
        .iter()
        .map(|v| {
            let is_pinned = pinned.as_ref() == Some(&v.manifest);
            let mut tags = String::new();
            if v.current {
                tags.push_str(" ·current");
            }
            if is_pinned {
                tags.push_str(" ·pinned");
            }
            if !v.stored {
                tags.push_str(" ·not stored");
            }
            let size = if v.size > 0 { human_size(v.size) } else { "-".into() };
            let mut style = Style::default();
            if v.current {
                style = style.fg(Color::Green);
            }
            if is_pinned {
                style = style.add_modifier(Modifier::BOLD);
            }
            ListItem::new(format!("{}   {:>9}{}", fmt_date(v.timeupdated), size, tags)).style(style)
        })
        .collect();
    let full = f.area();
    let h = (app.versions.len() as u16 + 2).clamp(3, full.height.saturating_sub(2));
    let area = centered_rect(full.width.saturating_sub(8).min(72), h, full);
    f.render_widget(Clear, area);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Versions of {name} — newest first "))
                .title_bottom(" enter pin · esc cancel "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.version_state);
}

/// The `R` requirement picker: toggle which other load-order mods the
/// selected mod requires.
pub(crate) fn draw_requirement_picker(f: &mut Frame, app: &mut App) {
    let Some(rs) = app.req_slot else { return };
    let name = app.slot_name(rs);
    let reqs = &app.slots[rs].requires;
    let cands = app.req_candidates();
    let items: Vec<ListItem> = cands
        .iter()
        .map(|&i| {
            let s = &app.slots[i];
            let checked = reqs.iter().any(|r| r.eq_ignore_ascii_case(&s.id));
            let mark = if checked { "[x]" } else { "[ ]" };
            let nm = app.slot_name(i);
            let style = if checked {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(format!("{mark} {nm}")).style(style)
        })
        .collect();
    let full = f.area();
    let h = (cands.len() as u16 + 2).clamp(3, full.height.saturating_sub(2));
    let area = centered_rect(full.width.saturating_sub(8).min(72), h, full);
    f.render_widget(Clear, area);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {name} requires… "))
                .title_bottom(" space toggle · esc done "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.req_state);
}

pub(crate) fn draw_status_popup(f: &mut Frame, app: &App) {
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
pub(crate) fn profile_span(app: &App) -> Span<'static> {
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

pub(crate) fn pane_block(title: Line<'static>, focused: bool) -> Block<'static> {
    let block = Block::default().borders(Borders::ALL).title(title);
    if focused {
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block
    }
}

pub(crate) fn highlight(focused: bool) -> (Style, &'static str) {
    if focused {
        (Style::default().add_modifier(Modifier::REVERSED), "▶ ")
    } else {
        (Style::default().add_modifier(Modifier::UNDERLINED), "  ")
    }
}

pub(crate) fn draw_available(f: &mut Frame, app: &mut App, area: Rect) {
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

pub(crate) fn draw_profile(f: &mut Frame, app: &mut App, area: Rect) {
    let rows: Vec<Row> = app
        .slots
        .iter()
        .enumerate()
        .map(|(n, s)| {
            let (mut label, mut style) = match s.idx {
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
                None => (format!("{} (missing)", s.id), Style::default().fg(Color::Red)),
            };
            let mut num_style = Style::default().fg(Color::Yellow);
            if !s.enabled {
                label = format!("{label}  (off)");
                style = Style::default().fg(Color::DarkGray);
                num_style = Style::default().fg(Color::DarkGray);
            } else {
                let unmet = app.unmet_requirements(s);
                if !unmet.is_empty() {
                    label = format!("{label}  ⚠ needs {}", unmet.join(", "));
                    style = Style::default().fg(Color::Red);
                }
            }
            Row::new(vec![
                Cell::from(format!("{:>3}", n + 1)).style(num_style),
                Cell::from(label).style(style),
            ])
        })
        .collect();
    let focused = app.focus == Pane::Profile;
    let (hl, sym) = highlight(focused);
    let off = app.slots.iter().filter(|s| !s.enabled).count();
    let count = if off > 0 {
        format!(" ({}, {off} off) ", app.slots.len())
    } else {
        format!(" ({}) ", app.slots.len())
    };
    let title = Line::from(vec![
        Span::raw(" Load order — "),
        profile_span(app),
        Span::raw(count),
    ]);
    let table = Table::new(rows, [Constraint::Length(3), Constraint::Min(20)])
        .block(pane_block(title, focused))
        .row_highlight_style(hl)
        .highlight_symbol(sym);
    f.render_stateful_widget(table, area, &mut app.prof_state);
}

/// The pool index of the mod selected in the focused pane, or the bare
/// uuid for a profile entry that isn't installed.
pub(crate) enum SidePanelSubject {
    Mod(usize),
    Unknown(String),
    None,
}

pub(crate) fn side_panel_subject(app: &App) -> SidePanelSubject {
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

pub(crate) fn draw_side_panel(f: &mut Frame, app: &mut App, area: Rect) {
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

pub(crate) fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

pub(crate) fn draw_profiles_popup(f: &mut Frame, app: &mut App) {
    app.refresh_preview();
    let full = f.area();
    let area = centered_rect(full.width.saturating_sub(8).min(96), 22, full);
    f.render_widget(Clear, area);
    let [body, footer_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let [names_area, preview_area] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(24)]).areas(body);

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
        .block(Block::default().borders(Borders::ALL).title(" Profiles "))
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
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        preview_area,
    );
    let footer = " enter apply · J/K or shift+↑↓ reorder · n new · r rename · e export · d delete · esc close";
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        footer_area,
    );
}

pub(crate) fn draw_name_input(f: &mut Frame, app: &mut App) {
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

