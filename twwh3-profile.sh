#!/usr/bin/env bash
#
# twwh3-profile — save, load, and manage Total War: WARHAMMER III mod/settings
# profiles by snapshotting the game's Roaming folder inside its Proton prefix.

set -euo pipefail

APPID=1142710

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

STEAM_ROOT="${STEAM_ROOT:-$(expand_tilde "$(cfg steam_root)")}"
STEAM_ROOT="${STEAM_ROOT:-$HOME/.local/share/Steam}"
DATA_DIR="${TWWH3_DATA:-$(expand_tilde "$(cfg data_dir)")}"
DATA_DIR="${DATA_DIR:-$HOME/Games/TotalWarWH3}"
PROFILES="${TWWH3_PROFILES:-$(expand_tilde "$(cfg snapshots)")}"
PROFILES="${PROFILES:-$DATA_DIR/profiles}"

PREFIX="$STEAM_ROOT/steamapps/compatdata/$APPID/pfx/drive_c/users/steamuser"
ROAMING="$PREFIX/AppData/Roaming/The Creative Assembly"

die() { echo "Error: $*" >&2; exit 1; }

usage() {
  cat <<EOF
Usage: ${0##*/} <command> [name]

Commands:
  save <name>     Snapshot the current game Roaming folder as a profile
  load <name>     Replace the game Roaming folder with a saved profile
  list            List saved profiles
  delete <name>   Delete a saved profile
  help            Show this help

Configuration (~/.config/twwh3-mods/config, shared with twwh3-mods):
  steam_root      Steam library containing the game
  data_dir        Base data dir (default: ~/Games/TotalWarWH3)
  snapshots       Snapshot storage dir (default: <data_dir>/profiles)

Environment (overrides config):
  STEAM_ROOT, TWWH3_DATA, TWWH3_PROFILES
EOF
}

cmd="${1:-}"
name="${2:-}"

require_name() {
  if [ -z "$name" ]; then
    usage >&2
    exit 1
  fi
  case "$name" in
    */*|.|..) die "invalid profile name: $name" ;;
  esac
}

case "$cmd" in

  save)
    require_name
    [ -d "$ROAMING" ] || die "game data not found at: $ROAMING
Has the game been run at least once?"

    dst="$PROFILES/$name"
    tmp="$PROFILES/.$name.tmp.$$"

    [ -d "$dst" ] && echo "Overwriting existing profile '$name'"

    # Copy into a temp dir first so an existing profile is only replaced
    # once the snapshot has fully succeeded.
    mkdir -p "$tmp"
    trap 'rm -rf "$tmp"' EXIT
    cp -a "$ROAMING" "$tmp/"
    rm -rf "$dst"
    mv "$tmp" "$dst"
    trap - EXIT

    echo "Saved profile '$name'."
    ;;

  load)
    require_name
    src="$PROFILES/$name/${ROAMING##*/}"
    [ -d "$src" ] || die "profile not found: $name"
    [ -d "$PREFIX" ] || die "Proton prefix not found at: $PREFIX
Has the game been installed and run at least once?"

    echo "Loading profile '$name'..."

    # Move the live folder aside instead of deleting it, so a failed copy
    # can be rolled back without losing the current game data.
    backup=""
    if [ -d "$ROAMING" ]; then
      backup="$ROAMING.bak.$$"
      mv "$ROAMING" "$backup"
    fi

    if cp -a "$src" "$ROAMING"; then
      if [ -n "$backup" ]; then
        rm -rf "$backup"
      fi
      echo "Done. Restart WH3."
    else
      rm -rf "$ROAMING"
      if [ -n "$backup" ]; then
        mv "$backup" "$ROAMING"
      fi
      die "copy failed; original game data restored"
    fi
    ;;

  list)
    if [ -d "$PROFILES" ] && [ -n "$(ls -A "$PROFILES" 2>/dev/null)" ]; then
      ls -1 "$PROFILES"
    else
      echo "No profiles saved yet."
    fi
    ;;

  delete)
    require_name
    [ -d "$PROFILES/$name" ] || die "profile not found: $name"
    rm -rf "${PROFILES:?}/$name"
    echo "Deleted profile '$name'."
    ;;

  help|-h|--help)
    usage
    ;;

  *)
    usage >&2
    exit 1
    ;;
esac
