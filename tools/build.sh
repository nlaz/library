#!/bin/sh
# Build the Swift helper tools. Kept out of cargo so the Rust workspace
# never requires a Swift toolchain; the binaries are optional at runtime
# (ingest skips the cleanup pass when tools/clean-pages is absent).
set -e
cd "$(dirname "$0")"
# clean-pages imports FoundationModels, which only exists in the macOS 26 SDK.
# The app's floor is 14 and this pass is opt-in (`--clean`) and skipped at
# runtime when absent, so on an older toolchain say so and stop rather than
# failing a build nothing depends on.
if [ "$(sw_vers -productVersion | cut -d. -f1)" -lt 26 ]; then
  echo "skipping tools/clean-pages: FoundationModels needs the macOS 26 SDK"
  exit 0
fi
swiftc -O -parse-as-library clean-pages/main.swift -o clean-pages/clean-pages
echo "built tools/clean-pages/clean-pages"
