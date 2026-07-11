//! twwh3-mods — TUI load-order manager for Total War: WARHAMMER III.
//!
//! A mod is a *folder* that gets mirrored into the game's data/ at launch:
//! a Steam Workshop item dir (workshop/content/<appid>/<steam_id>/) or a
//! local mod dir (local_mods/<name>/). Either may hold several .pack files plus
//! loose assets (movies, tables); the packs are listed in used_mods, the
//! whole folder rides along via the overlay. Local mods are first-class —
//! you don't need Steam Workshop at all.
//!
//! Two panes: "Available" lists every mod on disk (Workshop scan + local
//! folders); "Load order" is the ordered list that will be enabled. The
//! load order lives in profiles (TWWH3_MODLISTS, default
//! ~/Games/TotalWarWH3/modlists); the launcher's moddata.dat is read for
//! Workshop names and to seed a first-run load order, but is never
//! written — launching goes through twwh3-run's staging/overlay.
//!
//! Profile entries whose mod isn't installed are shown as missing, skipped
//! at launch, and preserved in the profile file.

use anyhow::{bail, Context, Result};
use ratatui_image::picker::Picker;
use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};

mod paths;
mod steam;
mod model;
mod overlay;
mod store;
mod app;
mod profiles;
mod launch;
mod versions;
mod bundle;
mod ui;
use paths::*;
use steam::*;
use overlay::*;
use app::{run, App};

const APPID: u32 = 1142710;
const GAME: &str = "warhammer3";


fn usage() {
    println!(
        "twwh3-mods — TUI mod load-order manager for Total War: WARHAMMER III\n\n\
         Usage: twwh3-mods [--list | --launch | --paths | used-mods]\n         \
                twwh3-mods export <profile> [dest-dir]\n         \
                twwh3-mods import <bundle.tar> [--as name]\n\n\
         Options:\n  \
           -l, --list   Print the load order and available mods, then exit\n  \
           --launch     Write used_mods.txt and start the game via Steam\n  \
           --paths      Print every resolved path and where config is read from\n  \
           used-mods    Dry run: print the exact ordered load order and the\n               \
           used_mods.txt / used_mods_overlay.txt a launch would pass to the\n               \
           game — no files written, nothing launched\n  \
           export       Pack a profile + its stored packs into one .tar\n  \
           import       Unpack a bundle into the store and install its profile\n  \
           -h, --help   Show this help\n\n\
         Keys:\n  \
           tab / h / l      switch pane   j/k or arrows        select\n  \
           space / enter    add to or remove from the load order\n  \
           J/K              reorder within the load order\n  \
           v                pick which stored version of the hovered\n                    \
           Workshop mod to use (with update dates)\n  \
           U                update all '(updated)' mods to the current\n                    \
           version (pins are otherwise kept on save)\n  \
           p                profiles (enter apply, n new, r rename,\n                    \
           e export, d delete)\n  \
           o                open the game's data/ folder as the game will\n                    \
           see it (mounts a merged preview; o again unmounts)\n  \
           ? help     s save     S status page     L launch     q quit\n\n\
         Launching (one-time setup):\n  \
           L / --launch write used_mods.txt into the game folder and run\n  \
           `steam -applaunch {APPID}` — Steam still does all the Proton work.\n  \
           To make the game (and Steam's own Play button) skip the CA\n  \
           launcher and use that exact file, set the game's Steam launch\n  \
           options to:\n\n    \
           twwh3-run %command%\n\n  \
           (twwh3-run ships alongside this tool.) Without it, the CA\n  \
           launcher opens as usual and uses the same mod list, minus\n  \
           version pinning.\n\n\
         Mods are folders:\n  \
           A mod is a folder mirrored into the game's data/. Workshop items\n  \
           (workshop/content/<appid>/<id>/) and local mods each are folders\n  \
           holding one or more .pack files (+ loose assets for local mods).\n  \
           For a local mod, make a subfolder of the local_mods dir\n  \
           (TWWH3_LOCAL) — e.g. local_mods/MyMod/ — put its .pack(s) at the\n  \
           folder root and any loose files (movies/, etc.) in matching\n  \
           subdirs;\n  \
           it appears under Available marked 'local'. No Steam Workshop\n  \
           needed. The load order lives in profiles; moddata.dat is read for\n  \
           Workshop names but never written (launch is overlay-only).\n\n\
         Versioning (Workshop only):\n  \
           Each Workshop mod is pinned to a Steam manifest (depot GID for an\n  \
           exact version); the item is copied into versioned_workshop_mods\n  \
           (keyed by id + manifest). Pins are STICKY: saving keeps every\n  \
           already-pinned mod on its version and only pins newly-added mods\n  \
           to what's installed now — so adding a mod never bumps the others.\n  \
           When Steam force-updates a pinned mod the load order marks it\n  \
           '(updated)' and L keeps loading the pinned (old) version. Press v\n  \
           on a Workshop mod to pick any stored version (shown with their\n  \
           Steam update dates), or U to move every '(updated)' mod to the\n  \
           current version; then s to save. Every version you use is kept in\n  \
           versioned_workshop_mods/<id>/<manifest>/ so you can switch back.\n  \
           Local mods aren't versioned (you own them).\n\n\
         Staging & overlay:\n  \
           On launch each mod folder is mirrored into a staging folder of\n  \
           symlinks (default: ~/Games/TotalWarWH3/staging) — Workshop files\n  \
           resolve through versioned_workshop_mods (current version, stored\n  \
           on demand, or the pinned one); local folders mirror as-is.\n  \
           `ls -lR` there shows the resolution. Because Workshop files\n  \
           resolve through that store,\n  \
           a launch never depends on the live workshop folder and an\n  \
           unsubscribed mod still plays.\n  \
           twwh3-run then merges the staging folder into the game's data/\n  \
           with fuse-overlayfs for the duration of the run (movie packs\n  \
           work; game files stay pristine). Without fuse-overlayfs it\n  \
           falls back to plain working-directory loading automatically.\n  \
           On a sandboxed (bwrap) Steam, twwh3-run can't mount the overlay\n  \
           itself; while the TUI is open it services the mount from outside\n  \
           the sandbox (launch with L, or keep twwh3-mods running).\n\n\
         Portable bundles:\n  \
           `export <profile>` writes <profile>.twwh3bundle.tar with the\n  \
           profile, its versioned Workshop packs, and its local mod folders\n  \
           (in the TUI, press e in the profiles popup — it lands in\n  \
           <data_dir>/bundles). Move or share that one file;\n  \
           `import <bundle.tar>` unpacks it back (verifying each pack against\n  \
           its sha256) and installs\n  \
           the profile without overwriting an existing one (--as renames).\n\n\
         Configuration (~/.config/twwh3-mods/config, `key = value` lines):\n  \
           steam_root  Steam library containing the game (default: ~/.local/share/Steam)\n  \
           data_dir    Base for this tool's data (default: ~/Games/TotalWarWH3)\n  \
           modlists    Profiles          (default: <data_dir>/modlists)\n  \
           versioned_workshop_mods  Cached Workshop mod versions\n                                    \
           (default: <data_dir>/versioned_workshop_mods)\n  \
           local_mods  Non-Workshop mod folders (default: <data_dir>/local_mods)\n  \
           staging     Launch symlinks   (default: <data_dir>/staging)\n  \
           moddata     Launcher mod list file    (default: derived from steam_root)\n  \
           workshop    Workshop content dir      (default: derived from steam_root)\n  \
           game_dir    Game install dir          (default: derived from steam_root)\n  \
           images      auto (default) | halfblocks | off\n  \
           overlay     on (default) | off — twwh3-run's data/ overlay\n  \
           open_with   command `o` opens folders with (default: xdg-open)\n\n  \
           Each key also has an env var that overrides it: STEAM_ROOT,\n  \
           TWWH3_DATA, TWWH3_MODLISTS, TWWH3_VERSIONED_WORKSHOP_MODS, TWWH3_LOCAL,\n  \
           TWWH3_STAGING, TWWH3_MODDATA, TWWH3_WORKSHOP, TWWH3_GAME,\n  \
           TWWH3_IMAGES, TWWH3_OVERLAY, TWWH3_OPEN. `twwh3-mods --paths`\n  \
           shows the resolved values."
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }

    // Bundle subcommands take positional args, so handle them before the
    // generic flag check below.
    match args.first().map(String::as_str) {
        Some("export") => {
            let name = args
                .get(1)
                .context("usage: twwh3-mods export <profile> [dest-dir]")?;
            let dest = args
                .get(2)
                .map(|s| expand_path(s))
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let (path, packs, missing) = App::export_bundle(name, &dest)?;
            let miss = if missing > 0 {
                format!(", {missing} without a stored pack")
            } else {
                String::new()
            };
            println!("Exported '{name}' → {} ({packs} packs{miss})", path.display());
            return Ok(());
        }
        Some("import") => {
            let file = args
                .get(1)
                .context("usage: twwh3-mods import <bundle.tar> [--as name]")?;
            let as_name = args
                .iter()
                .position(|a| a == "--as")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            let (name, verified) = App::import_bundle(&expand_path(file), as_name)?;
            println!(
                "Imported profile '{name}' ({verified} packs verified). \
                 Run twwh3-mods and press p to apply it."
            );
            return Ok(());
        }
        Some("used-mods") => {
            // Dry run: print the exact load order + used_mods contents a
            // launch would pass, without writing anything.
            let app = App::load(moddata_path(), None)?;
            print!("{}", app.used_mods_preview());
            return Ok(());
        }
        _ => {}
    }

    if let Some(bad) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "-l" | "--list" | "--launch" | "--paths"))
    {
        usage();
        bail!("unknown argument: {bad}");
    }

    if args.iter().any(|a| a == "--paths") {
        let cfg = config_file();
        let cfg_note = if cfg.exists() { "" } else { "  (not present)" };
        println!("config file:  {}{cfg_note}", cfg.display());
        println!("steam_root:   {}", steam_root().display());
        println!("data_dir:     {}", data_dir().display());
        println!("moddata:      {}", moddata_path().display());
        println!("workshop:     {}", workshop_dir().display());
        match game_install_dir() {
            Some(d) => println!("game_dir:     {}", d.display()),
            None => println!("game_dir:     (not found)"),
        }
        println!("modlists:     {}", modlists_dir().display());
        println!("versioned_workshop_mods: {}", versioned_mods_dir().display());
        println!("local_mods:   {}", local_mods_dir().display());
        println!("staging:      {}", staging_dir().display());
        println!(
            "images:       {}",
            setting("TWWH3_IMAGES", "images").unwrap_or_else(|| "auto".into())
        );
        return Ok(());
    }

    if args.iter().any(|a| a == "--launch") {
        let mut app = App::load(moddata_path(), None)?;
        app.launch();
        println!("{}", app.status);
        return Ok(());
    }

    if !args.is_empty() {
        // --list: no terminal queries, no TUI.
        let app = App::load(moddata_path(), None)?;
        let mut out = String::from("Load order:\n");
        for (n, s) in app.slots.iter().enumerate() {
            let (name, note) = match s.idx {
                Some(i) if !app.pool[i].missing => (
                    app.pool[i].name(),
                    if app.slot_updated(s) { "  (updated)" } else { "" },
                ),
                Some(i) => (app.pool[i].name(), "  (missing)"),
                None => (s.id.as_str(), "  (missing)"),
            };
            out.push_str(&format!("{:>3}  {name}{note}\n", n + 1));
        }
        out.push_str("\nAvailable:\n");
        for i in app.available() {
            let m = &app.pool[i];
            let note = if m.local { "  (local)" } else { "" };
            out.push_str(&format!("     {}{note}\n", m.name()));
        }
        // Ignore broken pipes from e.g. `--list | head`.
        use std::io::Write;
        let _ = std::io::stdout().write_all(out.as_bytes());
        return Ok(());
    }

    // Query the terminal for its graphics protocol (kitty/sixel/iTerm2)
    // before entering the alternate screen; fall back to half-blocks.
    //
    // TWWH3_IMAGES=halfblocks or =off skips the query: if a terminal never
    // answers it, ratatui-image leaks a reader thread that steals
    // keystrokes from the TUI for the rest of the session.
    let picker = match setting("TWWH3_IMAGES", "images").as_deref() {
        Some("off") => None,
        Some("halfblocks") => Some(Picker::from_fontsize((8, 16))),
        _ => Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)))),
    };
    let mut app = App::load(moddata_path(), picker)?;

    // Service overlay mount requests from twwh3-run (which can't mount
    // itself when the game runs inside Steam's bwrap sandbox).
    start_mount_listener();

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    stop_mount_listener();
    // Don't leave a preview overlay mounted behind us.
    if app.preview_mounted {
        if let Some(game) = game_install_dir() {
            let _ = Command::new("fusermount3")
                .args(["-u"])
                .arg(game.join("data"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    res
}
