#!/usr/bin/env bash
#
# twwh3-run — Steam launch-option shim for Total War: WARHAMMER III.
#
# Set the game's Steam launch options to:
#
#   twwh3-run %command%
#
# Steam expands %command% to the full Proton invocation ending in the CA
# launcher exe. This shim swaps that target for Warhammer3.exe and appends
# the used_mods.txt mod list (written by twwh3-mods or the CA launcher
# itself), so the game boots straight into the pinned load order with no
# launcher in between. Proton wrapping is untouched — every other argument
# passes through as-is.
#
# Mods are delivered by merging the staging folder (the folder of symlinks twwh3-mods
# builds at launch, one per enabled mod) into the game's data/ directory
# with a fuse-overlayfs mount for the duration of the run. The real data/
# stays pristine, Steam's "verify integrity" never sees the mods, and pack
# types that only load from data/ (movie packs) work. While the overlay is
# up the game is given the mod-lines-only list (used_mods_overlay.txt) so
# each pack is visible exactly once. If fuse-overlayfs is not installed
# (or `overlay = off` / TWWH3_OVERLAY=off is set), the game falls back to
# loading mods from the staging folder via the add_working_directory line
# in used_mods.txt — everything except movie packs works identically.
#
# Steam's working directory is the launcher subfolder, but the game
# resolves the mod-list argument relative to its working directory, so we
# hop to the game root before running. A short log of every invocation is
# kept at ~/.cache/twwh3-run.log.

set -euo pipefail

LOG_FILE="${XDG_CACHE_HOME:-$HOME/.cache}/twwh3-run.log"

log() {
  printf '%s %s\n' "$(date '+%F %T')" "$*" >>"$LOG_FILE" 2>/dev/null || true
}

# Shared config file (same one twwh3-mods reads): `key = value` lines.
# Env vars win over the config file, defaults fill the rest.
CONFIG_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/twwh3-mods/config"

cfg() {
  [ -f "$CONFIG_FILE" ] || return 0
  sed -n -E "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*[\"']?([^\"'#]*)[\"']?[[:space:]]*\$/\1/p" \
    "$CONFIG_FILE" | tail -n1
}

# Expand a literal leading ~/ in config values (the shell doesn't expand
# tildes read from files).
# shellcheck disable=SC2088
expand_tilde() {
  local v="${1:-}"
  case "$v" in
    "~/"*) printf '%s' "$HOME/${v#??}" ;;
    *) printf '%s' "$v" ;;
  esac
}

if [ "$#" -eq 0 ]; then
  echo "twwh3-run: no command given." >&2
  echo "Set the game's Steam launch options to: twwh3-run %command%" >&2
  exit 64
fi

gamedir=""
args=()
for a in "$@"; do
  case "$a" in
    *launcher/launcher.exe | *launcher\\launcher.exe)
      a="${a%launcher*launcher.exe}Warhammer3.exe"
      ;;
  esac
  case "$a" in
    */Warhammer3.exe)
      gamedir="${a%/Warhammer3.exe}"
      ;;
  esac
  args+=("$a")
done

if [ -n "$gamedir" ] && [ -d "$gamedir" ]; then
  cd "$gamedir"
elif [ -n "$gamedir" ]; then
  log "warning: game dir not found: $gamedir"
fi

DATA_DIR="${TWWH3_DATA:-$(expand_tilde "$(cfg data_dir)")}"
DATA_DIR="${DATA_DIR:-$HOME/Games/TotalWarWH3}"
STAGING="${TWWH3_STAGING:-$(expand_tilde "$(cfg staging)")}"
STAGING="${STAGING:-$DATA_DIR/staging}"
OVERLAY="${TWWH3_OVERLAY:-$(cfg overlay)}"
OVERLAY="${OVERLAY:-on}"

# Shared with twwh3-mods (this dir is visible inside Steam's sandbox — the
# log above already lives here). When we can't mount the overlay ourselves
# (a bwrapped Steam sets no_new_privs, which neuters setuid fusermount3),
# we ask a twwh3-mods listener running outside the sandbox to do it; its
# host-side mount propagates back in here.
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}"
MOUNT_REQ="$CACHE_DIR/twwh3-mount-request"
UNMOUNT_REQ="$CACHE_DIR/twwh3-unmount-request"
LISTENER_MARKER="$CACHE_DIR/twwh3-mount-listener"

# Ask the listener to mount, then wait for data/ to become a mountpoint
# (the host mount propagates into this sandbox). 0 on success.
mount_via_manager() {
  [ -f "$LISTENER_MARKER" ] || return 1
  command -v mountpoint >/dev/null || return 1
  printf '%s\n' "$data" >"$MOUNT_REQ" 2>/dev/null || return 1
  local n=0
  while [ "$n" -lt 50 ]; do
    mountpoint -q "$data" 2>/dev/null && return 0
    sleep 0.1
    n=$((n + 1))
  done
  return 1
}
unmount_via_manager() { printf '%s\n' "$data" >"$UNMOUNT_REQ" 2>/dev/null || true; }

mounted=""          # "" | direct | manager
modlist="used_mods.txt"
data="$PWD/data"
if [ -n "$gamedir" ] && [ -d "$data" ]; then
  # Clear a stale overlay left behind by a crashed previous run.
  if command -v mountpoint >/dev/null && mountpoint -q "$data"; then
    fusermount3 -u "$data" 2>/dev/null || unmount_via_manager
    log "cleared stale overlay on $data"
  fi
  if [ "$OVERLAY" != "off" ] && [ -d "$STAGING" ]; then
    if command -v fuse-overlayfs >/dev/null \
       && fuse-overlayfs -o "lowerdir=$STAGING:$data" "$data" 2>>"$LOG_FILE"; then
      # Direct mount (works on a non-sandboxed Steam).
      mounted=direct
      trap 'fusermount3 -u "$data" 2>/dev/null || true' EXIT
      log "overlay mounted directly: $STAGING merged into $data"
    elif mount_via_manager; then
      # Sandboxed Steam: the twwh3-mods listener mounted it on the host.
      mounted=manager
      trap 'unmount_via_manager' EXIT
      log "overlay mounted via twwh3-mods listener: $STAGING merged into $data"
    else
      log "no overlay (direct mount denied and no listener); loading mods from the staging folder"
    fi
    # With the packs visible in data/, loading them from the staging
    # working directory as well would present every pack twice and confuse
    # save-game mod matching — use the mod-lines-only list.
    if [ -n "$mounted" ] && [ -f used_mods_overlay.txt ]; then
      modlist="used_mods_overlay.txt"
    fi
  fi
fi

log "cwd=$PWD run: ${args[*]} $modlist;"
rc=0
"${args[@]}" "$modlist;" || rc=$?

case "$mounted" in
  direct)
    if fusermount3 -u "$data" 2>>"$LOG_FILE"; then
      log "overlay unmounted"
    else
      log "warning: could not unmount overlay on $data"
    fi
    trap - EXIT
    ;;
  manager)
    unmount_via_manager
    log "asked twwh3-mods listener to unmount overlay on $data"
    trap - EXIT
    ;;
esac
exit "$rc"
