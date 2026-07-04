//! Media "Studio" commands — generate social post copy and images for the Studio tile.
//! The generation primitives live in `llm`/`tools` so the chat agent can reuse them too.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::models::PlatformCopy;
use crate::AppState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratePostCopyRequest {
    pub topic: String,
    pub platforms: Vec<String>,
    pub tone: Option<String>,
}

/// Generate tailored post copy (3 variants per selected platform).
#[tauri::command]
pub async fn generate_post_copy(
    state: State<'_, AppState>,
    req: GeneratePostCopyRequest,
) -> Result<Vec<PlatformCopy>, String> {
    if req.topic.trim().is_empty() {
        return Err("Enter a topic first.".to_string());
    }
    crate::llm::generate_social_copy(&state, req.topic.trim(), &req.platforms, req.tone.as_deref()).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WritingAssistRequest {
    pub text: String,
    pub instruction: String,
}

/// Apply a writing instruction (improve / shorten / continue / write…) to a draft.
#[tauri::command]
pub async fn writing_assist(
    state: State<'_, AppState>,
    req: WritingAssistRequest,
) -> Result<String, String> {
    crate::llm::writing_assist(&state, &req.text, &req.instruction).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateDocRequest {
    /// "slides" or "document".
    pub doc_type: String,
    pub topic: String,
    pub slides: Option<u32>,
}

/// Generate a self-contained HTML slide deck or document for the Docs section.
#[tauri::command]
pub async fn generate_doc_html(
    state: State<'_, AppState>,
    req: GenerateDocRequest,
) -> Result<String, String> {
    crate::llm::generate_doc_html(&state, &req.doc_type, &req.topic, req.slides.unwrap_or(7)).await
}

/// Write generated HTML to the media/docs dir and open it in the default browser (where the
/// user can print → Save as PDF). Returns the saved file path.
#[tauri::command]
pub fn open_html(app: AppHandle, html: String, title: String) -> Result<String, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let docs_dir = data_dir.join("media").join("docs");
    fs::create_dir_all(&docs_dir).map_err(|e| e.to_string())?;

    let mut slug: String = title
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() { "studio-doc".to_string() } else { slug };

    let path = docs_dir.join(format!("{}.html", slug));
    fs::write(&path, html.as_bytes()).map_err(|e| e.to_string())?;

    if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg(&path)
            .status()
            .map_err(|e| format!("failed to open document: {}", e))?;
    }
    Ok(path.to_string_lossy().to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateCarouselRequest {
    pub topic: String,
    pub slides: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Generate carousel/graphic slides as inline-styled HTML strings (one per slide); the
/// frontend rasterizes each with html2canvas.
#[tauri::command]
pub async fn generate_carousel(
    state: State<'_, AppState>,
    req: GenerateCarouselRequest,
) -> Result<Vec<String>, String> {
    crate::llm::generate_carousel(
        &state,
        &req.topic,
        req.slides.unwrap_or(5),
        req.width.unwrap_or(1080),
        req.height.unwrap_or(1080),
    )
    .await
}

/// Save a rasterized PNG (base64, no data: prefix) into the Content library. Returns the path.
#[tauri::command]
pub fn studio_save_png(app: AppHandle, base64: String) -> Result<String, String> {
    let cleaned = base64.split(',').last().unwrap_or(&base64);
    let bytes = B64.decode(cleaned.trim()).map_err(|e| format!("invalid image data: {}", e))?;
    let dir = crate::tools::content_dir(&app)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let path = dir.join(format!("studio-graphic-{}.png", stamp));
    fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    let _ = app.emit("content-changed", ());
    Ok(path.to_string_lossy().to_string())
}

/// Generate an image for a post and place it in the gallery (asset library). Returns the
/// gallery file path; the frontend turns it into a data URL via `read_image_base64`.
#[tauri::command]
pub async fn studio_generate_image(
    app: AppHandle,
    state: State<'_, AppState>,
    prompt: String,
) -> Result<String, String> {
    if prompt.trim().is_empty() {
        return Err("Enter an image prompt.".to_string());
    }
    // The shared pipeline already saves into the Content library; just return that path and
    // refresh any open Content tile.
    let path = crate::tools::generate_image_to_path(&app, &state, prompt.trim(), None).await?;
    let _ = app.emit("content-changed", ());
    Ok(path.to_string_lossy().to_string())
}
