//! The versioned Workshop-mods store, staging mirror, and hashing.

use crate::model::walk_files;
use crate::paths::versioned_mods_dir;
use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Copy a Workshop mod's whole folder (packs, preview, any loose files)
/// into versioned_workshop_mods/<sid>/<manifest>/, skipping files already
/// present, so the stored version is a faithful copy that survives
/// unsubscribe. Records a sha256 sidecar per pack and a `.meta.json` with
/// the version's update time/size. Returns how many packs were newly
/// stored.
pub(crate) fn store_mod_version(sid: &str, manifest: &str, dir: &Path, timeupdated: u64, size: u64) -> Result<usize> {
    if manifest.is_empty() {
        return Ok(0);
    }
    let dst_dir = versioned_mods_dir().join(sid).join(manifest);
    let mut n = 0usize;
    for (rel, src) in walk_files(dir) {
        let dst = dst_dir.join(&rel);
        if dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let fname = rel.file_name().and_then(|s| s.to_str()).unwrap_or("f");
        // Copy to a temp name first so an interrupted copy can't be
        // mistaken for a complete stored file.
        let tmp = dst.with_file_name(format!(".{fname}.tmp"));
        fs::copy(&src, &tmp).with_context(|| format!("could not store {}", src.display()))?;
        fs::rename(&tmp, &dst)?;
        if src.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")) {
            // A content hash beside the pack lets a version be verified
            // independently of Steam (used when importing a bundle).
            if let Some(hash) = pack_sha256(&dst) {
                let _ = fs::write(sha256_sidecar(&dst), hash);
            }
            n += 1;
        }
    }
    write_version_meta(sid, manifest, timeupdated, size);
    Ok(n)
}

/// Per-version metadata stored alongside the files (never mirrored into
/// data/, since the mirror's file list comes from the live mod folder).
pub(crate) fn version_meta_path(sid: &str, manifest: &str) -> PathBuf {
    versioned_mods_dir().join(sid).join(manifest).join(".meta.json")
}

/// Record a stored version's Steam update time and size. Won't clobber an
/// existing record with unknown (0) values.
pub(crate) fn write_version_meta(sid: &str, manifest: &str, timeupdated: u64, size: u64) {
    let dir = versioned_mods_dir().join(sid).join(manifest);
    if !dir.is_dir() {
        return;
    }
    let path = version_meta_path(sid, manifest);
    if timeupdated == 0 && path.exists() {
        return;
    }
    let meta = serde_json::json!({
        "manifest": manifest,
        "timeupdated": timeupdated,
        "size": size,
        "stored_at": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    });
    let _ = fs::write(path, serde_json::to_string_pretty(&meta).unwrap_or_default());
}

/// (timeupdated, size) recorded for a stored version, 0 if unknown.
pub(crate) fn read_version_meta(sid: &str, manifest: &str) -> (u64, u64) {
    let Some(v) = fs::read_to_string(version_meta_path(sid, manifest))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return (0, 0);
    };
    (
        v.get("timeupdated").and_then(Value::as_u64).unwrap_or(0),
        v.get("size").and_then(Value::as_u64).unwrap_or(0),
    )
}

/// Format a Unix timestamp (seconds, UTC) as "YYYY-MM-DD HH:MM".
pub(crate) fn fmt_date(secs: u64) -> String {
    if secs == 0 {
        return "unknown".into();
    }
    let days = (secs / 86400) as i64;
    let sod = secs % 86400;
    // civil-from-days (Howard Hinnant's algorithm), UTC.
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", sod / 3600, (sod % 3600) / 60)
}

/// Streaming SHA-256 of a file, hex-encoded. `manifest` (Steam's depot
/// GID) names an exact *version*; this hashes the actual pack bytes.
pub(crate) fn pack_sha256(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    let mut hex = String::with_capacity(64);
    use std::fmt::Write as _;
    for b in hasher.finalize() {
        let _ = write!(hex, "{b:02x}");
    }
    Some(hex)
}

/// The `<pack>.sha256` sidecar path next to a vaulted pack.
pub(crate) fn sha256_sidecar(pack: &Path) -> PathBuf {
    let mut name = pack.file_name().unwrap_or_default().to_os_string();
    name.push(".sha256");
    pack.with_file_name(name)
}

/// The `.pack` file inside a vault version directory, if any.
pub(crate) fn find_stored_pack(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pack")))
}

/// The content hash of a vaulted pack, from its sidecar (or computed and
/// cached if the sidecar is absent — e.g. an older vault).
pub(crate) fn read_or_make_sha256(pack: &Path) -> Option<String> {
    let sidecar = sha256_sidecar(pack);
    if let Ok(s) = fs::read_to_string(&sidecar) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    let hash = pack_sha256(pack)?;
    let _ = fs::write(&sidecar, &hash);
    Some(hash)
}

/// Rebuild the staging folder to mirror `entries` — each (relative path,
/// source) becomes a symlink at staging/<rel> pointing at the source.
/// Parent dirs are created; later entries override earlier ones (load-
/// order precedence). The folder is rebuilt from scratch each launch.
pub(crate) fn rebuild_staging(staging: &Path, entries: &[(PathBuf, PathBuf)]) -> Result<()> {
    if staging.exists() {
        fs::remove_dir_all(staging)
            .with_context(|| format!("could not clear staging dir {}", staging.display()))?;
    }
    fs::create_dir_all(staging)
        .with_context(|| format!("could not create staging dir {}", staging.display()))?;
    for (rel, src) in entries {
        let dst = staging.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if dst.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&dst);
        }
        symlink(src, &dst)
            .with_context(|| format!("could not link {} into staging", rel.display()))?;
    }
    Ok(())
}

/// Where a specific vaulted pack version lives, if it exists.
pub(crate) fn stored_pack_path(sid: &str, manifest: &str, pack: &Path) -> Option<PathBuf> {
    let fname = pack.file_name()?;
    let p = versioned_mods_dir().join(sid).join(manifest).join(fname);
    p.exists().then_some(p)
}

pub(crate) fn human_size(b: u64) -> String {
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

/// (vaulted versions, total bytes) across the whole vault.
pub(crate) fn versioned_mods_stats() -> (usize, u64) {
    let mut versions = 0usize;
    let mut bytes = 0u64;
    for id_dir in fs::read_dir(versioned_mods_dir()).into_iter().flatten().flatten() {
        for man_dir in fs::read_dir(id_dir.path()).into_iter().flatten().flatten() {
            if !man_dir.path().is_dir() {
                continue;
            }
            versions += 1;
            for f in fs::read_dir(man_dir.path()).into_iter().flatten().flatten() {
                bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (versions, bytes)
}

