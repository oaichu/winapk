//! winapk-install: APK analysis (binary manifest + resource table),
//! adb install with ABI preference, AOT `speed` compile, icon→ICO,
//! desktop shortcut, and the installed-apps store.

pub mod apk;
pub mod arsc;
pub mod axml;
pub mod icon;
pub mod install;
pub mod shortcut;
pub mod store;

pub use install::{install_apk, InstallReport};
