#!/usr/bin/env bash
# Local release: build everything, stitch the sidecars into the .app, zip it,
# tag, and publish a GitHub Release.
#
#   scripts/release.sh [version] [--dry-run]
#
# With no version, releases whatever tauri.conf.json says. --dry-run stops
# after the zip (no commit, no tag, no push, no release) so the bundle can be
# inspected first.
#
# A zip, not a disk image. 0.1.0 shipped one by hand for a reason worth
# keeping: the app's own notarization comes back in minutes, but the DMG's sat
# in Apple's queue for seven hours, and a signed-but-unnotarized DMG is
# evaluated on mount — it hands back the blocked first launch that notarizing
# exists to remove. A zip is not evaluated that way at all. The ticket is
# stapled into the bundle before it is zipped, so the app opens clean the
# moment it is unpacked, offline included.
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
# https://github.com/nlaz/library/releases/latest/download/$ZIP_NAME forever.
ZIP_NAME="TheLibrary-macos-arm64.zip"
REPO="nlaz/library"

# Set SIGN_IDENTITY to a Developer ID Application identity to produce a
# signed, notarized, stapled build; leave it unset for the ad-hoc bundle
# (which Gatekeeper blocks on other machines — see the README).
#
#   security find-identity -v -p codesigning        # the identity string
#   xcrun notarytool store-credentials library-notary \
#     --apple-id you@example.com --team-id TEAMID --password <app-specific>
SIGN_IDENTITY="${SIGN_IDENTITY:-}"
NOTARY_PROFILE="${NOTARY_PROFILE:-library-notary}"

DRY_RUN=0
VERSION_ARG=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    *) VERSION_ARG="$arg" ;;
  esac
done

# --- preflight ---
for tool in cargo swift npm ditto codesign python3; do
  command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done
cargo tauri --version >/dev/null 2>&1 || { echo "missing: cargo-tauri (cargo install tauri-cli)" >&2; exit 1; }
[[ "$(uname -m)" == "arm64" ]] || { echo "this script produces an arm64 build; run on Apple silicon" >&2; exit 1; }
if [[ $DRY_RUN -eq 0 ]]; then
  command -v gh >/dev/null || { echo "missing: gh" >&2; exit 1; }
  gh auth status >/dev/null
  [[ -z "$(git -C "$ROOT" status --porcelain)" ]] || { echo "working tree not clean" >&2; exit 1; }
fi
# Fail on bad credentials now rather than after a multi-minute LTO build.
if [[ -n "$SIGN_IDENTITY" ]]; then
  security find-identity -v -p codesigning | grep -qF "$SIGN_IDENTITY" \
    || { echo "no codesigning identity matching: $SIGN_IDENTITY" >&2; exit 1; }
  xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null \
    || { echo "notarytool profile '$NOTARY_PROFILE' unusable (see store-credentials)" >&2; exit 1; }
else
  echo "warning: SIGN_IDENTITY unset — building ad-hoc signed, unnotarized" >&2
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
# Re-seal after modifying the bundle; sidecars first so the outer signature
# covers valid nested code. Hardened runtime (--options runtime) and a
# secure timestamp are both hard notarization requirements, and an
# unsigned nested binary fails the whole submission. No entitlements: the
# app isn't sandboxed, WKWebView's JIT lives in Apple-signed helper
# processes, and the sidecars are separate processes, not in-process
# dylibs — add an --entitlements plist only if a notarized build crashes.
if [[ -n "$SIGN_IDENTITY" ]]; then
  SIGN=(--force --options runtime --timestamp --sign "$SIGN_IDENTITY")
else
  SIGN=(--force --sign -)
fi
codesign "${SIGN[@]}" "$APP/Contents/Resources/librarian" "$APP/Contents/MacOS/library-ingest"
codesign "${SIGN[@]}" "$APP"
codesign --verify --deep --strict "$APP"

# --- notarize the app (minutes; Apple's service, needs network) ---
mkdir -p "$ROOT/dist"
if [[ -n "$SIGN_IDENTITY" ]]; then
  UPLOAD="$ROOT/dist/notarize.zip"
  ditto -c -k --keepParent "$APP" "$UPLOAD"  # a carrier for the upload only
  xcrun notarytool submit "$UPLOAD" --keychain-profile "$NOTARY_PROFILE" --wait
  rm -f "$UPLOAD"
  # The ticket goes into the bundle, not the carrier — which is why the
  # shipped zip has to be made after this, not reused from above.
  xcrun stapler staple "$APP"
  spctl -a -vv "$APP"                        # expect: source=Notarized Developer ID
fi

# --- the shipped artifact ---
ditto -c -k --keepParent "$APP" "$ROOT/dist/$ZIP_NAME"

if [[ $DRY_RUN -eq 1 ]]; then
  echo "dry run: built $ROOT/dist/$ZIP_NAME (v$VERSION); skipping tag + release"
  exit 0
fi

# --- tag + release ---
git -C "$ROOT" tag "v$VERSION"
git -C "$ROOT" push origin main "v$VERSION"
NOTES="macOS 14+, Apple silicon. Asking the librarian needs macOS 26."
[[ -n "$SIGN_IDENTITY" ]] || NOTES="$NOTES Unsigned — see the README for the first-launch step."
gh release create "v$VERSION" "$ROOT/dist/$ZIP_NAME" \
  --repo "$REPO" --title "The Library $VERSION" --notes "$NOTES"
echo "https://github.com/$REPO/releases/latest/download/$ZIP_NAME"
