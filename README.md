# Photo Cleaner (Rust / Android)

Swipe through your photos one at a time: **right = keep**, **left = mark for deletion**, small **Undo**.
Nothing is deleted until you confirm, and Android shows its own system confirmation as well.

## ⚠️ Verification status (read first)

**This project has NOT been compiled, run, or tested.** It was written in a sandbox that had no Rust
toolchain, no Android SDK/NDK, no `javac`, and no network access, so:

- no `cargo build` / `cargo ndk` was run
- no `cargo test` was run (unit tests for `src/store.rs` exist but are unexecuted)
- no Gradle build was run, **so there is no APK in this ZIP**
- nothing was run on a device or emulator

Expect to fix some compile errors on first build (most likely spots: egui/eframe 0.29 API details in
`src/android.rs`, jni 0.21 signatures). Run `./build.sh` and treat its output as the source of truth.

## What it is

- **Rust** (~all logic and UI): `egui` via `eframe` on a `NativeActivity` (glow/EGL). No WebView.
- **Java shim** (`MainActivity.java`, ~150 lines): the only Android APIs that need an Activity or
  Intent result: runtime permissions, `MediaStore` query/thumbnails, `MediaStore.createDeleteRequest`.
  Rust calls these over JNI; results come back through two native callbacks.
- **Dark mode only**, Pixel-style Material-ish palette.

## Behaviour

- Photos come from `MediaStore.Images` (newest first), thumbnails via `ContentResolver.loadThumbnail`
  sized to the screen. Only the current, next two, and previous photo are kept in memory.
- Swipe: card follows the finger, fades in a KEEP/DELETE label, flies off or springs back.
  The Keep / Delete / Undo buttons do the same.
- **Persistence** (`src/store.rs`): append-only log in app-private storage, keyed by MediaStore `_ID`:
  `K id` keep, `D id` mark, `U id` undo, `R id` forget after real deletion. Progress is "first photo
  with no decision", so restarts, new photos, and batch deletions never send you back to the start.
  Crash-truncated lines are tolerated; the log is compacted automatically.
- **Deleting**: the "Delete N" chip → in-app confirmation → Android's system dialog
  (`createDeleteRequest`). Only on `RESULT_OK` are photos forgotten; cancel keeps them marked.
  Batches are capped at 200 per request.
- **Permissions**: `READ_MEDIA_IMAGES` (Android 13+), `READ_EXTERNAL_STORAGE` (≤12L),
  `READ_MEDIA_VISUAL_USER_SELECTED` (Android 14+ partial access works). No `MANAGE_EXTERNAL_STORAGE`.

## Build

Requirements: rustup/cargo, JDK 17+, Android SDK + NDK (`ANDROID_HOME`, `ANDROID_NDK_HOME`),
Gradle 8.x (or generate a wrapper with `cd android && gradle wrapper`), and `cargo install cargo-ndk`.

```
./build.sh          # runs cargo test, builds .so for arm64-v8a + x86_64, then assembleDebug
```
Output: `dist/photo-cleaner-debug.apk`. Install with `adb install -r dist/photo-cleaner-debug.apk`.
Host-only tests: `cargo test --lib`.

## Known limitations

- Android 11+ (API 30) only. Debug-signed APK only; no release signing config.
- Images only (no videos), primary external volume only.
- No Gradle wrapper jar is included (could not be generated offline).
- No instrumented/UI tests; JNI bridge and gestures are untested.
- Unreviewed items are not scrolled/skipped by date; order is newest-first by `DATE_ADDED`.
- The in-app delete confirmation shows a count, not thumbnails.
