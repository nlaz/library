#!/usr/bin/env bash
# Check that the update artifacts in dist/ are actually installable, and
# then rehearse the whole update against a local server.
#
#   scripts/verify-update.sh              # check the artifacts
#   scripts/verify-update.sh --serve      # …and serve them at :8000
#
# This exists because of one specific failure. The updater does not launch
# an installer; it gunzips a tarball over a running app bundle and the next
# launch goes through Gatekeeper. If the code signature or the notarization
# ticket does not survive the round trip through tar, the update installs
# an app that macOS then refuses to open — and the person it happens to has
# no way back except downloading the zip by hand. That has to be caught
# here, not by them.
#
# With --serve, run the *bundled* app (not `cargo tauri dev` — a debug
# build has no bundle to replace and hides the update controls) against
# this server:
#
#   LIBRARY_UPDATE_ENDPOINT=http://127.0.0.1:8000/latest.json \
#     "target/release/bundle/macos/The Library.app/Contents/MacOS/library-app"
#
# then press Check for updates in Settings. For the check to find anything,
# dist/latest.json must name a version above the one you are running, and
# its `url` must point at http://127.0.0.1:8000/<tarball> rather than at
# GitHub — both are edits to make by hand, on a copy, for the rehearsal.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"
CONF="$ROOT/apps/library-app/tauri.conf.json"
TGZ="$DIST/TheLibrary-macos-arm64.app.tar.gz"

SERVE=0
[[ "${1:-}" == "--serve" ]] && SERVE=1

[[ -f "$TGZ" ]] || { echo "no $TGZ — run scripts/release.sh --dry-run first" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "== unpacking as the updater would =="
# The updater strips the first path component; this keeps it, so what lands
# in $WORK is the .app itself.
tar -xzf "$TGZ" -C "$WORK"
APP="$(find "$WORK" -maxdepth 1 -name '*.app' -print -quit)"
[[ -n "$APP" ]] || { echo "no .app at the root of the tarball" >&2; exit 1; }
echo "   $(basename "$APP")"

echo "== signature =="
codesign --verify --deep --strict --verbose=2 "$APP"

echo "== gatekeeper =="
# The line that matters is "source=Notarized Developer ID". Anything else
# means the ticket did not survive, and this build must not be published.
if ! spctl -a -vv "$APP" 2>&1 | tee "$WORK/spctl.txt"; then
  echo "!! Gatekeeper would reject this bundle" >&2
  exit 1
fi
grep -q "Notarized Developer ID" "$WORK/spctl.txt" || {
  echo "!! not notarized after the round trip — do not publish" >&2
  exit 1
}

echo "== stapled ticket =="
# Contents/CodeResources, a real file in the bundle rather than an extended
# attribute, which is why tar can carry it at all.
xcrun stapler validate "$APP"

echo "== update signature =="
SIG="$TGZ.sig"
if [[ ! -f "$SIG" ]]; then
  echo "   skipped: no $SIG (unsigned build)"
elif ! command -v minisign >/dev/null; then
  echo "   skipped: minisign not installed (brew install minisign)"
else
  # Both the stored signature and the configured public key are base64 over
  # minisign's own format, so they have to be decoded before minisign will
  # look at them.
  python3 - "$CONF" "$SIG" "$WORK" <<'PY'
import base64, json, sys
conf, sig, work = sys.argv[1:4]
pub = json.load(open(conf))["plugins"]["updater"]["pubkey"]
open(f"{work}/key.pub", "wb").write(base64.b64decode(pub))
open(f"{work}/file.sig", "wb").write(base64.b64decode(open(sig).read().strip()))
PY
  minisign -V -p "$WORK/key.pub" -x "$WORK/file.sig" -m "$TGZ"
  echo "   verifies against the key compiled into the app"
fi

echo
echo "artifacts are installable."

if [[ $SERVE -eq 1 ]]; then
  echo
  echo "serving $DIST at http://127.0.0.1:8000 — ^C to stop"
  echo "point a bundled build at http://127.0.0.1:8000/latest.json:"
  echo "  LIBRARY_UPDATE_ENDPOINT=http://127.0.0.1:8000/latest.json \\"
  echo "    \"$ROOT/target/release/bundle/macos/The Library.app/Contents/MacOS/library-app\""
  python3 -m http.server 8000 --directory "$DIST"
fi
