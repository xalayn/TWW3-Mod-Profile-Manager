#!/usr/bin/env bash
#
# twwh3-profile — save, load, and manage Total War: WARHAMMER III mod/settings
# profiles by snapshotting the game's Roaming folder inside its Proton prefix.

set -euo pipefail

APPID=1142710
STEAM_ROOT="${STEAM_ROOT:-$HOME/.local/share/Steam}"
PREFIX="$STEAM_ROOT/steamapps/compatdata/$APPID/pfx/drive_c/users/steamuser"
ROAMING="$PREFIX/AppData/Roaming/The Creative Assembly"
PROFILES="${TWWH3_PROFILES:-$HOME/Games/TotalWarWH3/profiles}"

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

Environment:
  STEAM_ROOT      Steam install root (default: ~/.local/share/Steam)
  TWWH3_PROFILES  Profile storage dir (default: ~/Games/TotalWarWH3/profiles)
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
