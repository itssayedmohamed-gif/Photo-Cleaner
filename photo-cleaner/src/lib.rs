//! Photo Cleaner: swipe through MediaStore photos, keep or mark for deletion.
//!
//! * `store`   – pure Rust, persistent decision log keyed by MediaStore `_ID`
//!               (host-testable with `cargo test`).
//! * `android` – egui UI + JNI bridge (Android only).

pub mod store;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    android::run(app);
}
