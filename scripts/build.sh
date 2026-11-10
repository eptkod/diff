#!/bin/bash
# Builds diff and puts the results in builds/ at the top of the repo.
# Usage: scripts/build.sh [app|exe|all|dmg|release]   (default: all)
#   all:     diff.app and diff-setup.exe
#   dmg:     diff.app, then diff.dmg made from it
#   release: diff.app, diff.dmg and diff-setup.exe
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILDS="$ROOT/builds"
WINDOWS_TARGET=x86_64-pc-windows-msvc
export PATH="/opt/homebrew/opt/rustup/bin:/opt/homebrew/opt/llvm/bin:$HOME/.cargo/bin:$PATH"

cd "$ROOT"
mkdir -p "$BUILDS"

build_app() {
  npx tauri build --bundles app
  rm -rf "${BUILDS:?}/diff.app"
  ditto "src-tauri/target/release/bundle/macos/diff.app" "$BUILDS/diff.app"
  echo "Built: $BUILDS/diff.app"
}

build_dmg() {
  local staging
  staging="$(mktemp -d)"
  ditto "$BUILDS/diff.app" "$staging/diff.app"
  # Lets people drag diff straight into Applications.
  ln -s /Applications "$staging/Applications"
  # hdiutil's own size guess is too small for the large voice model files.
  local size_mb
  size_mb=$(( $(du -sm "$staging" | cut -f1) * 3 / 2 + 20 ))
  hdiutil create -volname diff -srcfolder "$staging" -size "${size_mb}m" -ov -format UDZO "$BUILDS/diff.dmg"
  rm -rf "${staging:?}"
  echo "Built: $BUILDS/diff.dmg"
}

build_exe() {
  npx tauri build --runner cargo-xwin --target "$WINDOWS_TARGET" --bundles nsis
  local installer
  installer="$(ls -t "src-tauri/target/$WINDOWS_TARGET/release/bundle/nsis/"*-setup.exe | head -1)"
  cp "$installer" "$BUILDS/diff-setup.exe"
  echo "Built: $BUILDS/diff-setup.exe"
}

case "${1:-all}" in
  app) build_app ;;
  exe) build_exe ;;
  all) build_app; build_exe ;;
  dmg) build_app; build_dmg ;;
  release) build_app; build_dmg; build_exe ;;
  *) echo "Usage: $0 [app|exe|all|dmg|release]" >&2; exit 2 ;;
esac
