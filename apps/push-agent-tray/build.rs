//! Renders the app and window icons (`icons/icon.ico`, `icons/icon.png`, git-ignored) from
//! push-tray-core's glyph, then runs tauri-build, which embeds them.

use std::path::{Path, PathBuf};

use push_tray_core::icon;

const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
const PNG_SIZE: u32 = 256;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for source in ["src/icon.rs", "src/glyph.rs", "assets/glyph.svg"] {
        println!("cargo:rerun-if-changed=../../crates/push-tray-core/{source}");
    }
    let icons = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("icons");
    std::fs::create_dir_all(&icons).expect("create icons/");
    write_if_changed(&icons.join("icon.ico"), &icon::ico(&ICO_SIZES));
    write_if_changed(
        &icons.join("icon.png"),
        &icon::png(&icon::app_rgba(PNG_SIZE), PNG_SIZE),
    );
    tauri_build::build();
}

/// Writes `bytes` to `path` unless it already holds them (so that nothing rebuilds needlessly).
fn write_if_changed(path: &Path, bytes: &[u8]) {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return;
    }
    std::fs::write(path, bytes).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}
