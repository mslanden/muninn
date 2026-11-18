//! On-device speech-to-text for voice input, via the `muninn-transcriber` Swift sidecar
//! (speech/transcriber.swift, compiled by build.rs and bundled through tauri `externalBin`).
//! It uses Apple's SpeechAnalyzer on macOS 26 and SFSpeechRecognizer with on-device
//! recognition otherwise — no network and no key. Enabled by the `voice/local_transcription`
//! setting; `commands::voice::transcribe_wav` falls back to cloud whisper on any failure.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What the helper prints: the full text plus timed segments (only the text is used here).
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct HelperOutput {
    text: String,
    segments: Vec<serde_json::Value>,
}

/// Locate the compiled helper. Bundled builds have it next to the app binary (tauri
/// `externalBin` sidecar); dev and `cargo test` runs fall back to the build.rs output in
/// src-tauri/binaries/.
fn transcriber_helper_path() -> Option<PathBuf> {
    let names = [
        "muninn-transcriber".to_string(),
        format!("muninn-transcriber-{}", env!("MUNINN_TARGET_TRIPLE")),
    ];
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        for name in &names {
            let p = exe_dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries").join(&names[1]);
    dev.is_file().then_some(dev)
}

/// Transcribe an audio file on-device and return its text. Blocking — the first use per
/// locale downloads Apple's speech model, which can take a while.
pub(crate) fn transcribe_local(audio: &Path) -> Result<String, String> {
    let helper = transcriber_helper_path().ok_or_else(|| {
        "on-device transcriber helper not found (it is compiled by build.rs — rebuild with Xcode installed)".to_string()
    })?;
    let out = Command::new(&helper)
        .arg(audio)
        .output()
        .map_err(|e| format!("failed to start {}: {e}", helper.display()))?;
    if !out.status.success() {
        return Err(format!(
            "on-device transcriber failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let parsed: HelperOutput = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("on-device transcriber returned invalid JSON: {e}"))?;
    if parsed.text.is_empty() && parsed.segments.is_empty() {
        return Err("on-device transcriber produced an empty transcript".to_string());
    }
    Ok(parsed.text)
}
