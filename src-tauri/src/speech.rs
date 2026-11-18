//! On-device speech-to-text for voice input, via the `muninn-transcriber` Swift sidecar
//! (speech/transcriber.swift, compiled by build.rs and bundled through tauri `externalBin`).
//! It uses Apple's SpeechAnalyzer on macOS 26 and SFSpeechRecognizer with on-device
//! recognition otherwise — no network and no key. Enabled by the `voice/local_transcription`
//! setting; `commands::voice::transcribe_wav` falls back to cloud whisper on any failure.

use std::path::Path;
use std::process::Command;

/// What the helper prints: the full text plus timed segments (only the text is used here).
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct HelperOutput {
    text: String,
    segments: Vec<serde_json::Value>,
}

/// Transcribe an audio file on-device and return its text. Blocking — the first use per
/// locale downloads Apple's speech model, which can take a while.
pub(crate) fn transcribe_local(audio: &Path) -> Result<String, String> {
    let helper = crate::sidecar::helper_path("muninn-transcriber").ok_or_else(|| {
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
