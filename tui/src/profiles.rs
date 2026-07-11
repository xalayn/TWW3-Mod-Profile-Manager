//! Profiles: read/write modlists, apply/save, pins, drift.

use crate::app::App;
use crate::model::*;
use crate::paths::*;
use crate::steam::*;
use crate::store::*;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};


impl App {
    /// Resolve a profile's entries to load-order slots against the pool.
    pub(crate) fn slots_for_profile(&self, name: &str) -> Vec<Slot> {
        Self::read_modlist(name)
            .unwrap_or_default()
            .into_iter()
            .map(|e| {
                let idx = self.pool.iter().position(|m| m.id().eq_ignore_ascii_case(&e.id));
                Slot { id: e.id, idx, enabled: e.enabled, requires: e.requires }
            })
            .collect()
    }

    /// Seed the load order from the launcher's active mods (mapped to
    /// their folders) so users with no profile keep their current setup.
    pub(crate) fn slots_from_moddata(&self, meta: &HashMap<String, ModMeta>) -> Vec<Slot> {
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
                slots.push(Slot { id, idx: Some(idx), enabled: true, requires: Vec::new() });
            }
        }
        slots
    }

    /// Load the version pins recorded in a profile file.
    pub(crate) fn load_pins(&mut self, name: &str) {
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

    /// Human summary of what changed since the current profile was saved.
    pub(crate) fn drift_report(&self) -> Option<String> {
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
            "Since '{name}' was saved: {} — staying on pinned versions; v to pick, U to update all",
            parts.join("; ")
        ))
    }

    /// Save the current load order to its profile. The load order lives
    /// only in profiles now — the launcher's moddata is never written.
    pub(crate) fn save(&mut self) -> Result<()> {
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

    pub(crate) fn refresh_profiles(&mut self) {
        let mut names: Vec<String> = fs::read_dir(modlists_dir())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "json"))
                    .filter_map(|p| p.file_stem()?.to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        // Apply the saved order (.order); profiles not listed there (new
        // ones) keep their alphabetical spot after the ordered ones.
        let mut ordered = Vec::new();
        for want in fs::read_to_string(profile_order_file()).unwrap_or_default().lines() {
            let want = want.trim();
            if let Some(pos) = names.iter().position(|n| n == want) {
                ordered.push(names.remove(pos));
            }
        }
        ordered.extend(names);
        self.profiles = ordered;
        let sel = self
            .profile_list
            .selected()
            .unwrap_or(0)
            .min(self.profiles.len().saturating_sub(1));
        self.profile_list
            .select(if self.profiles.is_empty() { None } else { Some(sel) });
        self.preview_for = None;
    }

    /// Move the selected profile up/down in the popup and persist the order.
    pub(crate) fn move_profile(&mut self, delta: isize) {
        let Some(i) = self.profile_list.selected().filter(|&i| i < self.profiles.len()) else {
            return;
        };
        let j = i as isize + delta;
        if j < 0 || j as usize >= self.profiles.len() {
            return;
        }
        self.profiles.swap(i, j as usize);
        self.profile_list.select(Some(j as usize));
        let _ = fs::create_dir_all(modlists_dir());
        let _ = fs::write(profile_order_file(), self.profiles.join("\n"));
        self.preview_for = None;
    }

    pub(crate) fn selected_profile(&self) -> Option<&str> {
        self.profile_list
            .selected()
            .and_then(|i| self.profiles.get(i))
            .map(String::as_str)
    }

    pub(crate) fn read_modlist(name: &str) -> Result<Vec<MlEntry>> {
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
                    // Older profiles have no "active" field → default enabled.
                    enabled: e.get("active").and_then(Value::as_bool).unwrap_or(true),
                    requires: e
                        .get("requires")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                    steam_id,
                    manifest: e.get("manifest").and_then(Value::as_str).map(String::from),
                    sha256: e.get("sha256").and_then(Value::as_str).map(String::from),
                })
            })
            .collect())
    }

    /// Load the selected profile's mods for the popup's preview pane,
    /// resolving names against the pool.
    pub(crate) fn refresh_preview(&mut self) {
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

    /// Write the profile. Version pins are **sticky**: a mod already
    /// pinned keeps its pinned version, so saving (e.g. to add other mods)
    /// never bumps it — only a newly-added Workshop mod pins to the current
    /// installed version. Use v (pick a version) or U (all) to move on
    /// purpose. Returns how many pack versions were newly stored.
    pub(crate) fn write_modlist(&mut self, name: &str) -> Result<usize> {
        // Slots keep their id even when the mod is missing, so missing
        // mods survive a rewrite.
        let mut vaulted = 0usize;
        let mut mods: Vec<Value> = Vec::with_capacity(self.slots.len());
        for s in &self.slots {
            let mut o = serde_json::json!({ "id": s.id, "active": s.enabled });
            if !s.requires.is_empty() {
                o["requires"] = Value::from(s.requires.clone());
            }
            if let Some(entry) = s.idx.map(|i| &self.pool[i]) {
                o["local"] = Value::from(entry.local);
                if let Some(sid) = &entry.steam_id {
                    o["steam_id"] = Value::from(sid.clone());
                    let current = self
                        .ws
                        .get(sid)
                        .map(|w| w.manifest.clone())
                        .filter(|m| !m.is_empty());
                    // Keep an existing pin; a new mod pins to current.
                    let existing = self.pins.get(&s.id).cloned();
                    if let Some(manifest) = existing.clone().or_else(|| current.clone()) {
                        o["manifest"] = Value::from(manifest.clone());
                        // Store only when freshly pinning the current
                        // version; an existing pin is already stored (and
                        // its bytes may no longer exist live, so don't copy
                        // the newer files over the old version's folder).
                        if existing.is_none() {
                            let w = self.ws.get(sid);
                            let (tu, sz) = w.map(|w| (w.timeupdated, w.size)).unwrap_or((0, 0));
                            vaulted += store_mod_version(sid, &manifest, &entry.dir, tu, sz)?;
                        }
                        if current.as_deref() == Some(manifest.as_str()) {
                            if let Some(w) = self.ws.get(sid) {
                                o["timeupdated"] = Value::from(w.timeupdated);
                                o["size"] = Value::from(w.size);
                            }
                        }
                        if let Some(hash) = entry
                            .packs
                            .first()
                            .and_then(|p| stored_pack_path(sid, &manifest, p))
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

    pub(crate) fn save_profile(&mut self, name: &str) -> Result<()> {
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

    pub(crate) fn apply_profile(&mut self, name: &str) -> Result<()> {
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

    pub(crate) fn delete_profile(&mut self, name: &str) -> Result<()> {
        fs::remove_file(modlists_dir().join(format!("{name}.json")))?;
        if self.current_profile.as_deref() == Some(name) {
            self.set_current(None);
        }
        self.status = format!("Deleted profile '{name}'");
        Ok(())
    }

    pub(crate) fn rename_profile(&mut self, from: &str, to: &str) -> Result<()> {
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

}
