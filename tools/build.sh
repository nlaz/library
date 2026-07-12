#!/bin/sh
# Build the Swift helper tools. Kept out of cargo so the Rust workspace
# never requires a Swift toolchain; the binaries are optional at runtime
# (ingest skips the cleanup pass when tools/clean-pages is absent).
set -e
cd "$(dirname "$0")"
swiftc -O -parse-as-library clean-pages/main.swift -o clean-pages/clean-pages
echo "built tools/clean-pages/clean-pages"
