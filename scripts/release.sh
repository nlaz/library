#!/usr/bin/env bash
# Local release: build everything, stitch the sidecars into the .app, package
# it, tag, and publish a GitHub Release.
#
#   scripts/release.sh [version] [--dry-run]
#
# With no version, releases whatever tauri.conf.json says. --dry-run stops
# after the artifacts (no commit, no tag, no push, no release) so they can be
# inspected first.
#
# Three assets go out, and they are not redundant:
#
#   TheLibrary-macos-arm64.zip           what a person downloads
#   TheLibrary-macos-arm64.app.tar.gz    what an installed copy downloads
#   latest.json                          how it finds out there is one
#
# A zip, not a disk image, for the human path. 0.1.0 shipped one by hand for
# a reason worth keeping: the app's own notarization comes back in minutes,
# but the DMG's sat in Apple's queue for seven hours, and a
# signed-but-unnotarized DMG is evaluated on mount — it hands back the
# blocked first launch that notarizing exists to remove. A zip is not
# evaluated that way at all. The ticket is stapled into the bundle before it
# is packaged, so the app opens clean the moment it is unpacked, offline
# included.
#
# The tarball is the updater's format and is not negotiable — the plugin
# gunzips it, strips the leading path component, and swaps the result over
# the running bundle. It is built here rather than by `bundle.createUpdater\
# Artifacts`, because the bundler would tar the app before the sidecars are
# stitched in and before Apple has seen it: an update that installs an
# unnotarized bundle is worse than no update at all.
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
SITE="$ROOT/site/index.html"
APP="$ROOT/target/release/bundle/macos/The Library.app"
# Stable asset names, and no spaces in them: the site links
# https://github.com/nlaz/library/releases/latest/download/$ZIP_NAME forever,
# and latest.json has to name the tarball's URL exactly — GitHub rewrites
# spaces in asset filenames, so a "The Library.app.tar.gz" would 404.
ZIP_NAME="TheLibrary-macos-arm64.zip"
TGZ_NAME="TheLibrary-macos-arm64.app.tar.gz"
REPO="nlaz/library"
# The one platform key the updater will look for on this hardware. An Intel
# Mac finds nothing here and is told it is up to date, which is the right
# answer: only arm64 ships.
PLATFORM="darwin-aarch64"

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
  # The updater's own signature, which is not Apple's: minisign over the
  # tarball, checked against the pubkey compiled into every installed copy.
  # Lose this key and no copy already out there can ever be updated again.
  #
  #   export TAURI_SIGNING_PRIVATE_KEY_PATH=~/.tauri/thelibrary.key
  #   export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=...
  [[ -n "${TAURI_SIGNING_PRIVATE_KEY:-}${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]] \
    || { echo "set TAURI_SIGNING_PRIVATE_KEY_PATH (or _KEY) to sign the update" >&2; exit 1; }
  # Set-but-empty is a real answer (a key with no password); unset is not,
  # because the signer would stop and prompt halfway through a release.
  [[ -n "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD+set}" ]] \
    || { echo "set TAURI_SIGNING_PRIVATE_KEY_PASSWORD (empty is fine if the key has none)" >&2; exit 1; }
else
  echo "warning: SIGN_IDENTITY unset — building ad-hoc signed, unnotarized" >&2
  echo "warning: no update manifest will be produced" >&2
fi

# --- version ---
CUR="$(python3 -c "import json;print(json.load(open('$CONF'))['version'])")"
VERSION="${VERSION_ARG:-$CUR}"
if [[ $DRY_RUN -eq 1 && "$VERSION" != "$CUR" ]]; then
  echo "dry run: not bumping $CUR -> $VERSION; building $CUR" >&2
  VERSION="$CUR"
fi
if [[ $DRY_RUN -eq 0 ]]; then
  git -C "$ROOT" rev-parse "v$VERSION" >/dev/null 2>&1 \
    && { echo "tag v$VERSION already exists" >&2; exit 1; }
  # targeted edits, so both files keep their formatting. The landing page is
  # rewritten every release rather than only on a bump: it has drifted from
  # the shipped version before, and a download button labelled with a
  # version nobody is shipping is the kind of mistake only a stranger sees.
  [[ "$VERSION" == "$CUR" ]] \
    || sed -i '' "s/\"version\": \"$CUR\"/\"version\": \"$VERSION\"/" "$CONF"
  sed -i '' "/class=\"meta\"/s|v[0-9][0-9.]*|v$VERSION|" "$SITE"
  if [[ -n "$(git -C "$ROOT" status --porcelain -- "$CONF" "$SITE")" ]]; then
    git -C "$ROOT" add "$CONF" "$SITE"
    git -C "$ROOT" commit -m "release: v$VERSION"
  fi
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

# --- release notes ---
#
# One string, used twice: the GitHub release body, and `notes` in the
# updater manifest, which is what an installed copy shows before it
# updates itself. The requirements line is true of every build and is
# always appended; anything release-specific goes in a file named for the
# version, so cutting a release never means editing this script.
#
# Worth writing one whenever a release changes something a user would
# otherwise discover by noticing. 0.1.3 reclaims several gigabytes of page
# renders on first launch — every byte re-creatable, but nobody wants to
# find that out from Finder.
REQUIREMENTS="macOS 14+, Apple silicon. Asking the librarian needs macOS 26."
NOTES_FILE="$ROOT/docs/release-notes/$VERSION.md"
if [[ -f "$NOTES_FILE" ]]; then
  NOTES="$(cat "$NOTES_FILE")"$'\n\n'"$REQUIREMENTS"
else
  NOTES="$REQUIREMENTS"
fi
[[ -n "$SIGN_IDENTITY" ]] || NOTES="$NOTES Unsigned — see the README for the first-launch step."

# --- what a person downloads ---
ditto -c -k --keepParent "$APP" "$ROOT/dist/$ZIP_NAME"

# --- what an installed copy downloads ---
#
# The `.app` sits at the archive root: the updater strips the first path
# component and unpacks the remainder over the bundle it is running from.
#
# No mac metadata, and no xattrs. bsdtar would otherwise write AppleDouble
# `._` entries, and the plugin's Rust tar reader has no idea what those are
# — it would restore them as literal files inside the bundle and break the
# seal codesign just made. Nothing is lost by dropping them: the
# notarization ticket is a real file at Contents/CodeResources, and the
# signature lives in Contents/_CodeSignature.
( cd "$(dirname "$APP")" \
  && COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs \
       -czf "$ROOT/dist/$TGZ_NAME" "$(basename "$APP")" )

# --- how it finds out there is one ---
#
# Only for a signed build. An unsigned tarball has no minisign signature,
# every installed copy would refuse it, and publishing a manifest that
# points at something nobody can install is worse than publishing none.
ASSETS=("$ROOT/dist/$ZIP_NAME")
if [[ -n "$SIGN_IDENTITY" ]]; then
  rm -f "$ROOT/dist/$TGZ_NAME.sig"
  cargo tauri signer sign "$ROOT/dist/$TGZ_NAME" >/dev/null
  MANIFEST="$ROOT/dist/latest.json"
  VERSION="$VERSION" \
  URL="https://github.com/$REPO/releases/download/v$VERSION/$TGZ_NAME" \
  PLATFORM="$PLATFORM" NOTES="$NOTES" \
  SIG_FILE="$ROOT/dist/$TGZ_NAME.sig" \
  python3 - "$MANIFEST" <<'PY'
import datetime, json, os, sys

# pub_date must be RFC 3339; the updater parses it and refuses the whole
# manifest if it can't.
json.dump(
    {
        "version": os.environ["VERSION"],
        "notes": os.environ["NOTES"],
        "pub_date": datetime.datetime.now(datetime.timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z"),
        "platforms": {
            os.environ["PLATFORM"]: {
                "signature": open(os.environ["SIG_FILE"]).read().strip(),
                "url": os.environ["URL"],
            }
        },
    },
    open(sys.argv[1], "w"),
    indent=2,
)
PY
  ASSETS+=("$ROOT/dist/$TGZ_NAME" "$MANIFEST")
fi

if [[ $DRY_RUN -eq 1 ]]; then
  echo "dry run (v$VERSION): skipping tag + release. Built:"
  printf '  %s\n' "${ASSETS[@]}"
  echo "verify the tarball before trusting it — see scripts/verify-update.sh"
  exit 0
fi

# --- tag + release ---
git -C "$ROOT" tag "v$VERSION"
git -C "$ROOT" push origin main "v$VERSION"
gh release create "v$VERSION" "${ASSETS[@]}" \
  --repo "$REPO" --title "The Library $VERSION" --notes "$NOTES"
echo "https://github.com/$REPO/releases/latest/download/$ZIP_NAME"
