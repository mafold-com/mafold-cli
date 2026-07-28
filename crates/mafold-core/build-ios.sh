#!/usr/bin/env bash
# Build mafold-core into an iOS xcframework + UniFFI Swift bindings, and drop
# them into the app. Run by Codemagic before xcodegen; run locally once after
# cloning (the artifacts are gitignored — they're build outputs).
set -euo pipefail
cd "$(dirname "$0")"
APP_DIR="${1:-../mafold-ios}"

rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios

cargo build --release                                  # host dylib (for bindgen)
cargo build --release --target aarch64-apple-ios       # device (arm64)
cargo build --release --target aarch64-apple-ios-sim   # simulator (Apple Silicon)
cargo build --release --target x86_64-apple-ios        # simulator (Intel)

rm -rf generated headers MafoldCoreFFI.xcframework
mkdir -p generated headers
cargo run --release --bin uniffi-bindgen -- generate \
  --library target/release/libmafold_core.dylib --language swift --out-dir generated
# UniFFI's generated globals aren't Swift-6 strict-concurrency clean — mark them
# nonisolated(unsafe). `initializationResult`, plus the async/callback-interface
# globals (vtable / handleMap / uniffiContinuationHandleMap).
perl -i -pe 's/^(\s*)private var initializationResult/${1}private nonisolated(unsafe) var initializationResult/' generated/mafold_core.swift
perl -i -pe 's/\b((?:fileprivate |private |public )?(?:static )?)(var|let) (vtable|handleMap|uniffiContinuationHandleMap)\b/${1}nonisolated(unsafe) ${2} ${3}/' generated/mafold_core.swift
cp generated/mafold_coreFFI.h headers/
cp generated/mafold_coreFFI.modulemap headers/module.modulemap

# Fat simulator slice (arm64 + x86_64) so `-destination 'generic/platform=iOS
# Simulator'` links on both Apple-Silicon and Intel Macs without EXCLUDED_ARCHS.
# (Device stays arm64-only — all shipping iOS devices are arm64.)
mkdir -p target/ios-sim-universal/release
lipo -create \
  target/aarch64-apple-ios-sim/release/libmafold_core.a \
  target/x86_64-apple-ios/release/libmafold_core.a \
  -output target/ios-sim-universal/release/libmafold_core.a

xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libmafold_core.a -headers headers \
  -library target/ios-sim-universal/release/libmafold_core.a -headers headers \
  -output MafoldCoreFFI.xcframework

mkdir -p "$APP_DIR/Vendor" "$APP_DIR/Mafold/Generated"
rm -rf "$APP_DIR/Vendor/MafoldCoreFFI.xcframework"
cp -R MafoldCoreFFI.xcframework "$APP_DIR/Vendor/"
cp generated/mafold_core.swift "$APP_DIR/Mafold/Generated/mafold_core.swift"
echo "✅ mafold-core → $APP_DIR (xcframework + bindings)"
