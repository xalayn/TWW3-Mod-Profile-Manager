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
# Steam's working directory is the launcher subfolder, but the game
# resolves the mod-list argument relative to its working directory, so we
# hop to the game root before exec'ing. A short log of every invocation is
# kept at ~/.cache/twwh3-run.log.

set -euo pipefail

log() {
  printf '%s %s\n' "$(date '+%F %T')" "$*" \
    >>"${XDG_CACHE_HOME:-$HOME/.cache}/twwh3-run.log" 2>/dev/null || true
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

log "cwd=$PWD exec: ${args[*]} used_mods.txt;"
exec "${args[@]}" "used_mods.txt;"
