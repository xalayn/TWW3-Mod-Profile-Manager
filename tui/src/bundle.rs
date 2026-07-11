//! Portable export/import bundles.

use crate::app::{valid_profile_name, App};
use crate::model::*;
use crate::paths::*;
use crate::store::*;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// A profile name in `dir` that doesn't collide, appending " 2", " 3", …
/// to `base` if needed. Non-destructive: never overwrites an existing
/// profile.
pub(crate) fn unique_profile_name(dir: &Path, base: &str) -> String {
    if !dir.join(format!("{base}.json")).exists() {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|c| !dir.join(format!("{c}.json")).exists())
        .unwrap_or_else(|| base.to_string())
}

/// Append in-memory bytes to a tar under `name`.
pub(crate) fn tar_append_bytes<W: std::io::Write>(
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

impl App {
    /// Pack profile `name` and all of its vaulted packs into a single
    /// self-contained `<name>.twwh3bundle.tar` under `dest_dir`, so a mod
    /// setup can be moved to another machine or shared. Returns
    /// (archive path, packs included, mods without a vaulted pack).
    pub(crate) fn export_bundle(name: &str, dest_dir: &Path) -> Result<(PathBuf, usize, usize)> {
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
    pub(crate) fn import_bundle(path: &Path, as_name: Option<&str>) -> Result<(String, usize)> {
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
