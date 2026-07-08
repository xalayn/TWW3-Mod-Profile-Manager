#!/usr/bin/env bash
#
# Build and install the twwh3 tools without Nix.
#
#   ./install.sh                 # installs to ~/.local/bin
#   PREFIX=/usr/local ./install.sh   # installs to /usr/local/bin
#
# Requires: a Rust toolchain (https://rustup.rs) and bash.

set -euo pipefail

cd "$(dirname "$0")"

PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"

if ! command -v cargo >/dev/null; then
  echo "Error: cargo not found. Install Rust from https://rustup.rs," >&2
  echo "or download a prebuilt release instead:" >&2
  echo "  https://github.com/xalayn/TWW3-Mod-Profile-Manager/releases" >&2
  exit 1
fi

echo "Building twwh3-mods (release)..."
cargo build --release --manifest-path tui/Cargo.toml

install -Dm755 tui/target/release/twwh3-mods "$BIN/twwh3-mods"
install -Dm755 twwh3-profile.sh "$BIN/twwh3-profile"
install -Dm755 twwh3-run.sh "$BIN/twwh3-run"

echo "Installed to $BIN: twwh3-mods, twwh3-profile, twwh3-run"
case ":$PATH:" in
  *":$BIN:"*) ;;
  *) echo "Note: $BIN is not on your PATH." ;;
esac
