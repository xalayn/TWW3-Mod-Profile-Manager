//! Per-mod Workshop version selection.

use crate::app::{App, Mode};
use crate::model::*;
use crate::paths::*;
use crate::store::*;
use std::collections::HashSet;
use std::fs;


impl App {
    /// Store the current Workshop version of pool[i] and pin `id` to it,
    /// so `s` moves the mod up to it. Returns true if the pin changed.
    pub(crate) fn repin_current(&mut self, id: &str, i: usize) -> bool {
        let Some(sid) = self.pool[i].steam_id.clone() else { return false };
        let Some(w) = self.ws.get(&sid).filter(|w| !w.manifest.is_empty()) else {
            return false;
        };
        let (current, tu, sz) = (w.manifest.clone(), w.timeupdated, w.size);
        if self.pins.get(id) == Some(&current) {
            return false;
        }
        let dir = self.pool[i].dir.clone();
        let _ = store_mod_version(&sid, &current, &dir, tu, sz);
        self.pins.insert(id.to_string(), current);
        true
    }

    /// `U`: move every mod flagged (updated) to its current version.
    pub(crate) fn update_all(&mut self) {
        let targets: Vec<(String, usize)> = self
            .slots
            .iter()
            .filter(|s| self.slot_updated(s))
            .filter_map(|s| Some((s.id.clone(), s.idx?)))
            .collect();
        if targets.is_empty() {
            self.status = "No mods have a newer Workshop version".into();
            return;
        }
        let mut n = 0;
        for (id, i) in &targets {
            if self.repin_current(id, *i) {
                n += 1;
            }
        }
        if n > 0 {
            self.dirty = true;
            self.status = format!("{n} mods set to the current version — press s to save");
        }
    }

    /// `v`: open the version picker for the highlighted Workshop mod —
    /// every version we have stored plus the one Steam has installed now.
    pub(crate) fn open_version_picker(&mut self) {
        let Some(idx) = self.hovered_mod() else { return };
        let Some(sid) = self.pool[idx].steam_id.clone() else {
            self.status = "Local mods have no Workshop versions".into();
            return;
        };
        let current = self.ws.get(&sid).map(|w| w.manifest.clone()).filter(|m| !m.is_empty());
        let mut versions: Vec<VersionInfo> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for e in fs::read_dir(versioned_mods_dir().join(&sid)).into_iter().flatten().flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let Some(manifest) = e.file_name().to_str().map(String::from) else { continue };
            let (tu, sz) = read_version_meta(&sid, &manifest);
            let is_cur = current.as_deref() == Some(manifest.as_str());
            seen.insert(manifest.clone());
            versions.push(VersionInfo { manifest, timeupdated: tu, size: sz, stored: true, current: is_cur });
        }
        if let Some(cur) = &current {
            if seen.insert(cur.clone()) {
                let (tu, sz) = self.ws.get(&sid).map(|w| (w.timeupdated, w.size)).unwrap_or((0, 0));
                versions.push(VersionInfo {
                    manifest: cur.clone(),
                    timeupdated: tu,
                    size: sz,
                    stored: false,
                    current: true,
                });
            }
        }
        if versions.is_empty() {
            self.status = "No versions stored for this mod yet (save or launch to store one)".into();
            return;
        }
        versions.sort_by(|a, b| b.timeupdated.cmp(&a.timeupdated));
        let id = self.pool[idx].id().to_string();
        let sel = self
            .pins
            .get(&id)
            .and_then(|p| versions.iter().position(|v| &v.manifest == p))
            .or_else(|| versions.iter().position(|v| v.current))
            .unwrap_or(0);
        self.version_mod = Some(idx);
        self.versions = versions;
        self.version_state.select(Some(sel));
        self.mode = Mode::VersionPicker;
    }

    /// Pin the selected mod to the highlighted version (storing it first
    /// if needed) and return to Browse. A pin only lives in the profile,
    /// so it only takes effect — and only marks the profile modified — for
    /// a mod that's actually in the load order.
    pub(crate) fn pick_version(&mut self) {
        self.mode = Mode::Browse;
        let (Some(idx), Some(sel)) = (self.version_mod, self.version_state.selected()) else {
            return;
        };
        let Some(v) = self.versions.get(sel) else { return };
        let (manifest, stored, tu) = (v.manifest.clone(), v.stored, v.timeupdated);
        let Some(sid) = self.pool[idx].steam_id.clone() else { return };
        let id = self.pool[idx].id().to_string();
        let name = self.pool[idx].name().to_string();
        if !self.slots.iter().any(|s| s.idx == Some(idx)) {
            self.status = format!("'{name}' isn't in the load order — add it first to pin a version");
            return;
        }
        // Make sure the chosen version is on disk so it resolves at launch.
        if !stored {
            let dir = self.pool[idx].dir.clone();
            let (t, s) = self.ws.get(&sid).map(|w| (w.timeupdated, w.size)).unwrap_or((0, 0));
            let _ = store_mod_version(&sid, &manifest, &dir, t, s);
        }
        if self.pins.get(&id) != Some(&manifest) {
            self.dirty = true;
        }
        let is_current = self.ws.get(&sid).map(|w| w.manifest.as_str()) == Some(manifest.as_str());
        self.pins.insert(id, manifest);
        self.status = if is_current {
            format!("'{name}' pinned to the current version ({}) — s to save", fmt_date(tu))
        } else {
            format!("'{name}' pinned to the {} version — s to save", fmt_date(tu))
        };
    }

}
