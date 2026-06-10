//! Voice: whisper transcription, TTS, recording.

use tauri::{AppHandle, State};

use base64::{engine::general_purpose::STANDARD as B64, Engine};

use crate::AppState;
use crate::db::*;
use crate::models::*;

#[tauri::command]
pub async fn transcribe_whisper(
    state: State<'_, AppState>,
    req: TranscribeRequest,
) -> Result<TranscribeResponse, String> {
    let api_key = get_app_setting_value(&state, "llm/openai_api_key")?
        .or_else(|| get_app_setting_value(&state, "llm/minimax_api_key").ok().flatten())
        .ok_or_else(|| "OpenAI API key not configured (set llm/openai_api_key in settings)".to_string())?;

    let audio_bytes = B64.decode(&req.audio_base64).map_err(|e| format!("base64 decode error: {e}"))?;

    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.webm")
        .mime_str("audio/webm")
        .map_err(|e| e.to_string())?;

    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    let resp = state
        .http
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("whisper request failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("whisper API error: {body}"));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = json["text"].as_str().unwrap_or("").to_string();
    Ok(TranscribeResponse { text })
}

#[tauri::command]
pub async fn tts_speak(
    state: State<'_, AppState>,
    req: TtsRequest,
) -> Result<TtsResponse, String> {
    match req.provider.as_str() {
        "kokoro" => {
            let body = serde_json::json!({
                "model": "kokoro",
                "input": req.text,
                "voice": "af_heart"
            });
            let resp = state
                .http
                .post("http://127.0.0.1:8003/v1/audio/speech")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("kokoro request failed: {e}"))?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("kokoro error: {body}"));
            }
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("audio/mp3")
                .to_string();
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(TtsResponse {
                audio_base64: B64.encode(&bytes),
                content_type,
            })
        }
        "fishspeech" => {
            let body = serde_json::json!({
                "model": "fishaudio",
                "input": req.text,
                "voice": "default"
            });
            let resp = state
                .http
                .post("http://127.0.0.1:8080/v1/audio/speech")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("fishspeech request failed: {e}"))?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("fishspeech error: {body}"));
            }
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("audio/mp3")
                .to_string();
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(TtsResponse {
                audio_base64: B64.encode(&bytes),
                content_type,
            })
        }
        "minimax" => {
            let api_key = first_non_empty_setting(&state, &["llm/minimax_api_key"])
                .map_err(|e| e.to_string())?
                .or_else(|| std::env::var("MINIMAX_API_KEY").ok())
                .ok_or_else(|| "MiniMax API key not configured.".to_string())?;

            let voice_id = first_non_empty_setting(&state, &["tts/voice_id"])
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| "English_Graceful_Lady".to_string());

            let body = serde_json::json!({
                "model": "speech-2.8-hd",
                "text": req.text,
                "stream": false,
                "language_boost": "auto",
                "output_format": "hex",
                "voice_setting": {
                    "voice_id": voice_id,
                    "speed": 1.0,
                    "vol": 1.0,
                    "pitch": 0
                },
                "audio_setting": {
                    "sample_rate": 32000,
                    "bitrate": 128000,
                    "format": "mp3",
                    "channel": 1
                }
            });

            let resp = state
                .http
                .post("https://api.minimax.io/v1/t2a_v2")
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("MiniMax TTS request failed: {e}"))?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("MiniMax TTS error: {body}"));
            }

            let result: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

            if let Some(code) = result["base_resp"]["status_code"].as_i64() {
                if code != 0 {
                    let msg = result["base_resp"]["status_msg"].as_str().unwrap_or("unknown error");
                    return Err(format!("MiniMax TTS error {}: {}", code, msg));
                }
            }

            let hex_audio = result["data"]["audio"]
                .as_str()
                .ok_or_else(|| "No audio data in MiniMax TTS response".to_string())?;

            // Decode hex to bytes, then base64 encode for frontend
            let audio_bytes: Vec<u8> = (0..hex_audio.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex_audio[i..i + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|e| format!("hex decode error: {e}"))?;

            Ok(TtsResponse {
                audio_base64: B64.encode(&audio_bytes),
                content_type: "audio/mp3".to_string(),
            })
        }
        "system" => {
            let text = req.text.clone();
            std::thread::spawn(move || {
                let _ = std::process::Command::new("say").arg(&text).status();
            });
            Ok(TtsResponse {
                audio_base64: String::new(),
                content_type: String::new(),
            })
        }
        _ => Err(format!("unknown TTS provider: {}", req.provider)),
    }
}

#[tauri::command]
pub fn start_voice_recording(state: State<AppState>, app: AppHandle) -> Result<(), String> {
    if state.audio.is_recording() {
        return Err("Already recording".to_string());
    }
    state.audio.start(app);
    Ok(())
}

#[tauri::command]
pub fn stop_voice_recording(state: State<AppState>) -> Result<(), String> {
    state.audio.stop();
    Ok(())
}

#[tauri::command]
pub async fn transcribe_wav(
    state: State<'_, AppState>,
    wav_base64: String,
) -> Result<TranscribeResponse, String> {
    let api_key = get_app_setting_value(&state, "llm/openai_api_key")?
        .or_else(|| get_app_setting_value(&state, "llm/minimax_api_key").ok().flatten())
        .ok_or_else(|| "OpenAI API key not configured".to_string())?;

    let audio_bytes = B64.decode(&wav_base64).map_err(|e| format!("base64 decode error: {e}"))?;

    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;

    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    let resp = state
        .http
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("whisper request failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("whisper API error: {body}"));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = json["text"].as_str().unwrap_or("").to_string();
    Ok(TranscribeResponse { text })
}
