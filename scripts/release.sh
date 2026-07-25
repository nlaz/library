#!/usr/bin/env bash
# Local release: build everything, stitch the sidecars into the .app, make a
# DMG, tag, and publish a GitHub Release.
#
#   scripts/release.sh [version] [--dry-run]
#
# With no version, releases whatever tauri.conf.json says. --dry-run stops
# after the DMG (no commit, no tag, no push, no release) so the bundle can be
# inspected first.
#
# The sidecars are stitched in *after* `tauri build` on purpose: Tauri's
# externalBin/resources are validated at compile time, which would break
# `cargo tauri dev`, clippy, and CI on any machine that hasn't built the
# Swift sidecar. Post-build copying keeps dev and CI untouched; the copy
# destinations are exactly where chat.rs and ingest.rs already look
# (Contents/Resources/librarian, Contents/MacOS/library-ingest).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONF="$ROOT/apps/library-app/tauri.conf.json"
APP="$ROOT/target/release/bundle/macos/The Library.app"
# Stable asset name: the site links
# https://github.com/nlaz/library/releases/latest/download/$DMG_NAME forever.
DMG_NAME="TheLibrary-macos-arm64.dmg"
REPO="nlaz/library"

DRY_RUN=0
VERSION_ARG=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    *) VERSION_ARG="$arg" ;;
  esac
done

# --- preflight ---
for tool in cargo swift npm hdiutil codesign python3; do
  command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done
cargo tauri --version >/dev/null 2>&1 || { echo "missing: cargo-tauri (cargo install tauri-cli)" >&2; exit 1; }
[[ "$(uname -m)" == "arm64" ]] || { echo "this script produces an arm64 build; run on Apple silicon" >&2; exit 1; }
if [[ $DRY_RUN -eq 0 ]]; then
  command -v gh >/dev/null || { echo "missing: gh" >&2; exit 1; }
  gh auth status >/dev/null
  [[ -z "$(git -C "$ROOT" status --porcelain)" ]] || { echo "working tree not clean" >&2; exit 1; }
fi

# --- version ---
CUR="$(python3 -c "import json;print(json.load(open('$CONF'))['version'])")"
VERSION="${VERSION_ARG:-$CUR}"
if [[ "$VERSION" != "$CUR" ]]; then
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "dry run: not bumping $CUR -> $VERSION; building $CUR" >&2
    VERSION="$CUR"
  else
    # targeted edit keeps the file's formatting
    sed -i '' "s/\"version\": \"$CUR\"/\"version\": \"$VERSION\"/" "$CONF"
    git -C "$ROOT" add "$CONF"
    git -C "$ROOT" commit -m "release: v$VERSION"
  fi
fi
if [[ $DRY_RUN -eq 0 ]] && git -C "$ROOT" rev-parse "v$VERSION" >/dev/null 2>&1; then
  echo "tag v$VERSION already exists" >&2
  exit 1
fi

# --- build (slow: fat LTO, minutes) ---
[[ -d "$ROOT/apps/web/node_modules" ]] || npm --prefix "$ROOT/apps/web" install
swift build -c release --package-path "$ROOT/apps/librarian"
cargo build --release -p library-ingest
# web build runs via beforeBuildCommand (tauri v2 hook CWD is apps/)
( cd "$ROOT/apps/library-app" && cargo tauri build --bundles app )

# --- stitch sidecars where chat.rs / ingest.rs already look ---
cp "$ROOT/apps/librarian/.build/release/librarian" "$APP/Contents/Resources/librarian"
cp "$ROOT/target/release/library-ingest" "$APP/Contents/MacOS/library-ingest"
# re-seal ad-hoc after modifying the bundle; sidecars first so the outer
# signature covers valid nested code
codesign --force --sign - "$APP/Contents/Resources/librarian" "$APP/Contents/MacOS/library-ingest"
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"

# --- DMG (hdiutil; tauri's dmg step can't run after stitching) ---
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
mkdir -p "$ROOT/dist"
hdiutil create -volname "The Library" -srcfolder "$STAGE" -ov -format UDZO "$ROOT/dist/$DMG_NAME"

if [[ $DRY_RUN -eq 1 ]]; then
  echo "dry run: built $ROOT/dist/$DMG_NAME (v$VERSION); skipping tag + release"
  exit 0
fi

# --- tag + release ---
git -C "$ROOT" tag "v$VERSION"
git -C "$ROOT" push origin main "v$VERSION"
gh release create "v$VERSION" "$ROOT/dist/$DMG_NAME" \
  --repo "$REPO" --title "The Library $VERSION" \
  --notes "macOS 26+, Apple silicon. Unsigned — right-click the app and choose Open on first launch."
echo "https://github.com/$REPO/releases/latest/download/$DMG_NAME"
