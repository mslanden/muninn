//! Gallery and content-folder media commands.

use std::fs;
use tauri::AppHandle;

use base64::{engine::general_purpose::STANDARD as B64, Engine};

use crate::models::*;
use crate::tools::*;

#[tauri::command]
pub fn open_content_dir(app: AppHandle) -> Result<bool, String> {
    let dir = content_dir(&app)?;
    std::process::Command::new("open")
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub fn get_content_path(app: AppHandle) -> Result<ContentPathRecord, String> {
    let dir = content_dir(&app)?;
    Ok(ContentPathRecord {
        content_dir: dir.display().to_string(),
    })
}

#[tauri::command]
pub fn get_content_files(app: AppHandle) -> Result<Vec<MediaItemRecord>, String> {
    let dir = content_dir(&app)?;
    list_content_files(&dir)
}

#[tauri::command]
pub fn delete_content_file(app: AppHandle, req: MediaDeleteRequest) -> Result<bool, String> {
    let dir = content_dir(&app)?;
    let candidate = dir.join(&req.name);
    if !candidate.starts_with(&dir) {
        return Err("invalid content path".to_string());
    }
    if !candidate.exists() {
        return Ok(false);
    }
    fs::remove_file(candidate).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub fn import_content_files(app: AppHandle, req: ContentImportRequest) -> Result<Vec<String>, String> {
    let dir = content_dir(&app)?;
    let mut imported = Vec::new();
    for src_path in &req.paths {
        let src = std::path::Path::new(src_path);
        if !src.exists() || !src.is_file() {
            continue;
        }
        let file_name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
        let mut dest = dir.join(&file_name);
        // Handle name collisions
        if dest.exists() {
            let stem = src.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
            let mut i = 1u32;
            loop {
                dest = dir.join(format!("{} ({}){}", stem, i, ext));
                if !dest.exists() { break; }
                i += 1;
            }
        }
        if fs::copy(src, &dest).is_ok() {
            imported.push(dest.file_name().unwrap_or_default().to_string_lossy().to_string());
        }
    }
    Ok(imported)
}

#[tauri::command]
pub fn read_content_text(app: AppHandle, req: ContentReadRequest) -> Result<ContentReadResponse, String> {
    let dir = content_dir(&app)?;
    let file_path = std::path::Path::new(&req.path);
    let canonical = file_path.canonicalize().map_err(|e| e.to_string())?;
    let canon_dir = dir.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(&canon_dir) {
        return Err("path outside content directory".to_string());
    }
    let bytes = fs::read(&canonical).map_err(|e| e.to_string())?;
    let content = if bytes.len() > 65536 {
        String::from_utf8_lossy(&bytes[..65536]).to_string() + "\n\n... (truncated)"
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    let name = canonical.file_name().unwrap_or_default().to_string_lossy().to_string();
    Ok(ContentReadResponse { content, name })
}

#[tauri::command]
pub fn open_content_file(app: AppHandle, req: ContentReadRequest) -> Result<bool, String> {
    let dir = content_dir(&app)?;
    let file_path = std::path::Path::new(&req.path);
    let canonical = file_path.canonicalize().map_err(|e| e.to_string())?;
    let canon_dir = dir.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(&canon_dir) {
        return Err("path outside content directory".to_string());
    }
    std::process::Command::new("open")
        .arg(&canonical)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub fn read_image_base64(path: String) -> Result<String, String> {
    let bytes = fs::read(&path).map_err(|e| format!("Failed to read image: {}", e))?;
    let b64 = B64.encode(&bytes);
    let ext = path.rsplit('.').next().unwrap_or("png").to_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    Ok(format!("data:{};base64,{}", mime, b64))
}

// ==================== AI FEEDS ====================
