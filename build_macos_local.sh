#!/usr/bin/env bash

# This script builds the Gale macOS application locally.
# `cargo tauri build --bundles app` command builds the application.
# `codesign` command performs a naive signing of the application bundle to avoid macos flags it as damaged.
# `xattr` command removes the quarantine attribute.

# The end user will still have to explicitly allow the application to run from System Preferences > Security & Privacy > Security after the first attempt to run it.
#   The same goes for libdoorstop*.dylib which will be flagged as untrusted until at the first attempt to run it, after which it can be trusted from the location above.

# Build the application
echo "Building the application bundle"
cargo tauri build --bundles app

# Sign the application
echo "Signing the application bundle"
codesign --force --deep --sign - src-tauri/target/release/bundle/macos/Gale.app

echo "Removing the quarantine attribute from the application bundle"
xattr -cr src-tauri/target/release/bundle/macos/Gale.app

echo "Done"
# Open the folder containing the application bundle
open ./src-tauri/target/release/bundle/macos