//! Locating the Swift helper programs that build.rs compiles and tauri bundles through
//! `externalBin` (`muninn-transcriber`, `muninn-maps`).

use std::path::{Path, PathBuf};

/// Path of the helper `name`. Bundled builds have it next to the app binary; dev and
/// `cargo test` runs fall back to the build.rs output in src-tauri/binaries/ (which carries
/// the target-triple suffix tauri strips when bundling).
pub(crate) fn helper_path(name: &str) -> Option<PathBuf> {
    let names = [name.to_string(), format!("{}-{}", name, env!("MUNINN_TARGET_TRIPLE"))];
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        for n in &names {
            let p = exe_dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries").join(&names[1]);
    dev.is_file().then_some(dev)
}
