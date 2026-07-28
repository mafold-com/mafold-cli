#!/usr/bin/env bash
# Build mafold-core into Android per-ABI shared libs (.so) + UniFFI Kotlin
# bindings, and drop them into the Android app. The Android counterpart of
# build-ios.sh — same shape: build a host lib for the bindgen metadata read,
# cross-compile per target, generate the language binding, copy into the app.
# Run by CI before the Gradle build; run locally once after cloning (the
# artifacts are gitignored build outputs).
#
# Prereqs (see mafold-adr/README.md):
#   - Rust + the Android targets (this script adds them).
#   - cargo-ndk   : cargo install cargo-ndk
#   - Android NDK : export ANDROID_NDK_HOME=/path/to/ndk/<ver>   (or set
#                   ANDROID_HOME and let cargo-ndk find the latest installed NDK)
set -euo pipefail
cd "$(dirname "$0")"
APP_DIR="${1:-../mafold-adr}"
JNILIBS="$APP_DIR/app/src/main/jniLibs"
KOTLIN_OUT="$APP_DIR/app/src/main/java"

# ABIs we ship. arm64-v8a + armeabi-v7a = real devices; x86_64 + x86 = emulators.
# Override for faster dev builds, e.g. ANDROID_ABIS="arm64-v8a x86_64".
read -r -a ABIS <<< "${ANDROID_ABIS:-arm64-v8a armeabi-v7a x86_64 x86}"

rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android

# 1) Host lib — built only so uniffi-bindgen can read the FFI metadata (the same
#    trick build-ios.sh uses with the host .dylib). Extension differs per OS.
cargo build --release
HOSTLIB=""
for cand in \
  target/release/libmafold_core.so \
  target/release/libmafold_core.dylib \
  target/release/mafold_core.dll; do
  [ -f "$cand" ] && HOSTLIB="$cand" && break
done
[ -n "$HOSTLIB" ] || { echo "❌ host cdylib not found in target/release"; exit 1; }

# 2) Per-ABI .so via cargo-ndk → jniLibs/<abi>/libmafold_core.so (what iOS does
#    with lipo + xcframework; on Android the per-ABI .so layout IS the package).
rm -rf "$JNILIBS"
mkdir -p "$JNILIBS"
NDK_ARGS=()
for abi in "${ABIS[@]}"; do NDK_ARGS+=(-t "$abi"); done
cargo ndk "${NDK_ARGS[@]}" -o "$JNILIBS" build --release

# 3) UniFFI Kotlin bindings (package uniffi.mafold_core) → app/src/main/java/uniffi/
#    (the Kotlin analog of mafold-ios/Mafold/Generated/mafold_core.swift).
rm -rf generated-kotlin
mkdir -p generated-kotlin
cargo run --release --bin uniffi-bindgen -- generate \
  --library "$HOSTLIB" --language kotlin --out-dir generated-kotlin
# UniFFI's Kotlin backend emits the Rust method `WsHandle::close` as
# `override fun `close`()`, which COLLIDES with the auto-generated
# AutoCloseable.close() → "Conflicting overloads". Rename the Rust method to
# `stop()` (and drop `override`). The backtick form is unique to that Rust
# method, so this only touches WsHandle; iOS (Swift) / web (wasm) are unaffected.
# Mirrors how build-ios.sh perl-patches the generated Swift for Swift-6.
perl -i -pe 's/^(\s*)override fun `close`\(\)/${1}fun `stop`()/' \
  generated-kotlin/uniffi/mafold_core/mafold_core.kt
mkdir -p "$KOTLIN_OUT/uniffi"
rm -rf "$KOTLIN_OUT/uniffi/mafold_core"
cp -R generated-kotlin/uniffi/mafold_core "$KOTLIN_OUT/uniffi/"

echo "✅ mafold-core → $APP_DIR (jniLibs .so per ABI + Kotlin bindings)"
