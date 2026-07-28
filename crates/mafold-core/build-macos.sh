#!/usr/bin/env bash
# Build mafold-core into a macOS xcframework + UniFFI Swift bindings, and drop
# them into the native Mac app. This mirrors build-ios.sh but targets macOS
# directly, with a universal static library for Apple Silicon + Intel.
set -euo pipefail
cd "$(dirname "$0")"
APP_DIR="${1:-../mafold-mac}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"

rustup target add aarch64-apple-darwin x86_64-apple-darwin

cargo build --release
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

rm -rf generated headers MafoldCoreFFI-macos.xcframework
mkdir -p generated headers target/macos-universal/release
cargo run --release --bin uniffi-bindgen -- generate \
  --library target/release/libmafold_core.dylib --language swift --out-dir generated

# Keep parity with build-ios.sh: UniFFI's generated globals are not Swift-6
# strict-concurrency clean, so mark them explicitly.
perl -i -pe 's/^(\s*)private var initializationResult/${1}private nonisolated(unsafe) var initializationResult/' generated/mafold_core.swift
perl -i -pe 's/\b((?:fileprivate |private |public )?(?:static )?)(var|let) (vtable|handleMap|uniffiContinuationHandleMap)\b/${1}nonisolated(unsafe) ${2} ${3}/' generated/mafold_core.swift
cp generated/mafold_coreFFI.h headers/
cp generated/mafold_coreFFI.modulemap headers/module.modulemap

lipo -create \
  target/aarch64-apple-darwin/release/libmafold_core.a \
  target/x86_64-apple-darwin/release/libmafold_core.a \
  -output target/macos-universal/release/libmafold_core.a

xcodebuild -create-xcframework \
  -library target/macos-universal/release/libmafold_core.a -headers headers \
  -output MafoldCoreFFI-macos.xcframework

mkdir -p "$APP_DIR/Vendor" "$APP_DIR/MafoldMac/Generated"
rm -rf "$APP_DIR/Vendor/MafoldCoreFFI.xcframework"
cp -R MafoldCoreFFI-macos.xcframework "$APP_DIR/Vendor/MafoldCoreFFI.xcframework"
cp generated/mafold_core.swift "$APP_DIR/MafoldMac/Generated/mafold_core.swift"
echo "mafold-core -> $APP_DIR (macOS xcframework + bindings)"
