//! Launch path: resolve load order, used_mods, overlay, status page.

use crate::app::App;
use crate::model::*;
use crate::overlay::*;
use crate::paths::*;
use crate::steam::*;
use crate::store::*;
use crate::ui::{path_line, StatusLine};
use crate::APPID;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};


impl App {
    /// Gather everything for the status page (S): resolved paths, the
    /// launch/overlay plumbing, and the current mod/profile state.
    pub(crate) fn build_status(&self) -> Vec<StatusLine> {
        let mut v = Vec::new();

        v.push(StatusLine::new("twwh3-mods", concat!("v", env!("CARGO_PKG_VERSION")), None));
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

    /// The load order resolved to concrete mods, in launch order. Shared
    /// by launch (`write_used_mods`) and the dry-run (`used_mods_preview`)
    /// so the two never drift. Read-only: for a not-yet-vaulted Workshop
    /// version it returns the vault path it *will* load from; the caller
    /// materializes it. Missing mods are skipped.
    pub(crate) fn resolve_load_order(&self) -> Vec<ResolvedMod> {
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
    pub(crate) fn workshop_source(&self, entry: &ModEntry, s: &Slot) -> (Option<String>, PackSource) {
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
    pub(crate) fn staging_entries(&self, resolved: &[ResolvedMod]) -> Vec<(PathBuf, PathBuf)> {
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
    pub(crate) fn used_mods_lines(resolved: &[ResolvedMod]) -> Vec<String> {
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
    pub(crate) fn write_used_mods(&self) -> Result<(usize, usize)> {
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
                    let (tu, sz) = self.ws.get(sid).map(|w| (w.timeupdated, w.size)).unwrap_or((0, 0));
                    let _ = store_mod_version(sid, m, &e.dir, tu, sz);
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
    pub(crate) fn used_mods_preview(&self) -> String {
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
    pub(crate) fn launch(&mut self) {
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
    pub(crate) fn poll_overlay_status(&mut self) {
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
    pub(crate) fn toggle_data_view(&mut self) {
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

}
