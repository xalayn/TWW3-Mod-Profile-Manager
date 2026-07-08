# TWW3 Mod Profile Manager

Mod management tools for **Total War: WARHAMMER III** on Linux (Steam +
Proton). Three tools, one shared config:

| Tool | What it does |
|---|---|
| `twwh3-mods` | TUI mod manager: load order, profiles, version pinning, launch |
| `twwh3-run` | Steam launch-option shim that boots the game with your mod list, skipping the CA launcher |
| `twwh3-profile` | Full-folder snapshots of the game's settings/saves (Roaming folder) |

A **mod is a folder** mirrored into the game's `data/` — a Steam Workshop
item or a local mod dir, either of which may hold several `.pack` files
plus loose assets. Local mods are first-class; you don't need Steam
Workshop at all.

## Features

- **Two-pane TUI** — every mod (Steam Workshop items + local folders) on
  the left, your ordered load order on the right. Multi-pack mods show a
  pack count; local mods are marked `local`.
- **Mods are folders** — a mod is `workshop/content/<appid>/<id>/` or a
  `local_mods/<name>/` folder. Put its `.pack`(s) at the folder root and any
  loose files (movies, tables) in matching subdirs; the whole folder is
  mirrored into `data/` at launch. The load order lives in profiles; the
  launcher's `moddata.dat` is read for Workshop names but never written.
- **Profiles** — named mod lists you can switch between, and rename
  (`r`). Mods missing on disk are remembered, flagged, and skipped
  rather than lost.
- **Version pinning (Workshop)** — saving a profile records each Workshop
  mod's exact version (depot manifest) plus a `sha256`, and copies the
  item into a local store (`versioned_workshop_mods/<id>/<manifest>/`).
  When Steam force-updates a mod, the TUI flags it `(updated)` and launches
  your pinned version from the store instead. Local mods aren't stored —
  you own their files.
- **Portable setups** — Workshop mods launch from that store, so a setup
  never depends on the live Workshop folder (an unsubscribed mod still
  plays). `export` a profile to a single `.twwh3bundle.tar` (profile + its
  Workshop packs + local mod folders) to move it to another machine or
  share it; `import` unpacks it back, verifying each pack's `sha256`.
- **Local mods** — make a subfolder under `~/Games/TotalWarWH3/local_mods`
  per mod; it shows up alongside Workshop mods, no copying into the game dir.
- **Staging** — at launch each mod folder is mirrored into a staging
  folder of symlinks (`~/Games/TotalWarWH3/staging`): Workshop packs
  resolve through the versioned-mods store (current or pinned version), local folders
  mirror as-is. `ls -lR` there shows the whole resolution.
- **Overlay mounting** — `twwh3-run` merges the staging folder into the game's
  `data/` directory with a fuse-overlayfs mount for the duration of the
  run. Mods that only load from `data/` (movie packs) just work, the
  real game files are never touched, and Steam's "verify integrity"
  stays clean. If `fuse-overlayfs` isn't installed, it automatically
  falls back to working-directory loading (everything but movie packs
  works the same).
- **Sandboxed Steam (bwrap)** — on setups where Steam runs games inside a
  bubblewrap sandbox (e.g. NixOS's FHS build), `twwh3-run` can't mount the
  overlay itself (the sandbox's `no_new_privs` disables setuid
  `fusermount3`). When the TUI is open it acts as a listener and performs
  the mount from *outside* the sandbox — it propagates back in — so the
  overlay still works. Launch with `L` (or keep `twwh3-mods` running) to
  use it; otherwise it falls back to working-directory loading.
- **Mod thumbnails** in terminals with image support (kitty, sixel,
  iTerm2 protocol; half-blocks elsewhere).

## Install

### Prebuilt (no toolchain needed)

Download the latest tarball from
[Releases](https://github.com/xalayn/TWW3-Mod-Profile-Manager/releases)
and put the three files somewhere on your `PATH`:

```sh
tar xf twwh3-tools-*.tar.gz
cp twwh3-tools-*/twwh3-{mods,run,profile} ~/.local/bin/
```

The `twwh3-mods` binary is static (musl) and runs on any x86_64 Linux.

### From source

Needs a [Rust toolchain](https://rustup.rs) and bash:

```sh
./install.sh                     # installs to ~/.local/bin
PREFIX=/usr/local ./install.sh   # or elsewhere
```

### Nix / home-manager

The repo is a flake exporting `twwh3-mods`, `twwh3-profile`,
`twwh3-run`, a combined default package, and an overlay:

```nix
inputs.twwh3.url = "github:xalayn/TWW3-Mod-Profile-Manager";
# then e.g.
home.packages = [ inputs.twwh3.packages.${pkgs.system}.default ];
```

## One-time setup: launching without the CA launcher

`L` in the TUI (or `twwh3-mods --launch`) starts the game through Steam,
so Proton is handled exactly as if you pressed Play. For the game to
pick up your mod list (and pinned versions), set the game's Steam launch
options to:

```
twwh3-run %command%
```

(Use the full path to `twwh3-run` if `~/.local/bin` isn't on the PATH
Steam sees.) Without this, the CA launcher opens as usual and uses the
same mod list, minus version pinning.

For the overlay mount, install `fuse-overlayfs` from your distro
(`apt/dnf/pacman install fuse-overlayfs`; the Nix package already
bundles it). It's optional — without it mods still load, only movie
packs need it. `~/.cache/twwh3-run.log` shows what happened on each
launch.

## Usage

```sh
twwh3-mods                       # the TUI
twwh3-mods --list                # print load order + available mods
twwh3-mods --launch              # write used_mods.txt and start the game
twwh3-mods --paths               # show every resolved path
twwh3-mods used-mods             # dry run: print the exact load order the
                                 #   game will be passed (no writes/launch)
twwh3-mods export <profile>      # pack a profile + its packs into a .tar
twwh3-mods import <bundle.tar>   # unpack a bundle into the store + profiles
```

Keys: `tab`/`h`/`l` switch pane · `j`/`k` select · `space`/`enter`
add/remove · `J`/`K` reorder · `p` profiles · `s` save · `S` status
page · `o` open the merged `data/` view · `L` launch · `?` help · `q`
quit. Press `?` for the full key list; in the profiles popup, `n` new,
`r` rename, `e` export, `d` delete.

`o` opens the game's `data/` folder in your file manager *as the game
will see it*: if the overlay is up (game running) it opens the live
merged view, otherwise it mounts a preview of the current load order
first (press `o` again to unmount).

The status page (`S`) is the first thing to check when something's off:
it shows every resolved path, whether the Steam launch options and the
overlay requirements (fuse-overlayfs, fusermount3, /dev/fuse) are in
place, version-pin drift, and versioned-mods store usage.

Settings snapshots (saves, campaign state, options — everything in the
game's Roaming folder):

```sh
twwh3-profile save vanilla-campaign
twwh3-profile load vanilla-campaign
twwh3-profile list
```

## Configuration

Optional. Copy [config.example](config.example) to
`~/.config/twwh3-mods/config` and uncomment what you want to change —
Steam library location, data directories, thumbnail rendering, etc.
Environment variables override the file; run `twwh3-mods --paths` to see
what resolves to what.

By default everything lives under `~/Games/TotalWarWH3/` (profiles,
versioned_workshop_mods, local mods, staging, snapshots) and Steam is
expected at `~/.local/share/Steam`.
