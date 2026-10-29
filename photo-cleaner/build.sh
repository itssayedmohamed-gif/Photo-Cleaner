#!/usr/bin/env bash
# Builds the Rust library for Android, then the APK. Requires:
#   rustup + cargo, Android SDK (ANDROID_HOME), Android NDK (ANDROID_NDK_HOME),
#   JDK 17+, Gradle 8.x (or android/gradlew), and `cargo install cargo-ndk`.
set -euo pipefail
cd "$(dirname "$0")"

echo "==> Host unit tests (persistence logic)"
cargo test --lib

echo "==> Rust targets"
rustup target add aarch64-linux-android x86_64-linux-android

echo "==> Native library"
cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build --release

echo "==> APK"
cd android
if [ -x ./gradlew ]; then ./gradlew assembleDebug; else gradle assembleDebug; fi
cd ..
mkdir -p dist
cp android/app/build/outputs/apk/debug/app-debug.apk dist/photo-cleaner-debug.apk
echo "APK: dist/photo-cleaner-debug.apk"
