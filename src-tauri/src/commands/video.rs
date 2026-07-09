//! Video commands for the Studio Video tab: probe / analyze / plan / render / cancel.
//! Paths here come from the OS file picker (user-chosen), so they are not workspace-
//! scoped; the agent-facing video tools in `tools.rs` ARE workspace-scoped.

use std::path::PathBuf;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::db::with_db;
use crate::video::{self, EditPlan, SilenceRange, Transcript, VideoProbe};
use crate::AppState;

#[derive(Serialize)]
pub struct FilterPresetInfo {
    pub key: String,
    pub label: String,
}

#[tauri::command]
pub fn video_filter_presets() -> Vec<FilterPresetInfo> {
    video::FILTER_PRESETS
        .iter()
        .map(|(key, label, _)| FilterPresetInfo { key: key.to_string(), label: label.to_string() })
        .collect()
}

#[tauri::command]
pub async fn video_probe(path: String) -> Result<VideoProbe, String> {
    tauri::async_runtime::spawn_blocking(move || video::probe_video(&PathBuf::from(path)))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn video_thumbnails(path: String, count: Option<u32>) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        video::thumbnails(&PathBuf::from(path), count.unwrap_or(8))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct VideoAnalysis {
    pub probe: VideoProbe,
    pub silences: Vec<SilenceRange>,
    /// Source ranges left over after cutting the detected silences (padded).
    pub suggested_segments: Vec<video::Segment>,
    pub transcript: Option<Transcript>,
    /// Set when transcription was requested but failed (e.g. no API key) — analysis
    /// still succeeds so silence-based editing keeps working.
    pub transcript_error: Option<String>,
    /// Detected filler-word/stutter cut ranges (source seconds). Empty when there's no
    /// transcript (no word timings to scan).
    pub filler_ranges: Vec<SilenceRange>,
    /// Source ranges left over after cutting BOTH the detected silences and the filler
    /// ranges above (padded). Empty when there's no transcript.
    pub filler_keep_segments: Vec<video::Segment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoAnalyzeRequest {
    pub path: String,
    /// Skip the (network) transcription pass; probe + silences only.
    pub skip_transcript: Option<bool>,
}

#[tauri::command]
pub async fn video_analyze(
    state: State<'_, AppState>,
    req: VideoAnalyzeRequest,
) -> Result<VideoAnalysis, String> {
    let path = PathBuf::from(&req.path);
    let path_clone = path.clone();
    let (probe, silences) = tauri::async_runtime::spawn_blocking(move || {
        let probe = video::probe_video(&path_clone)?;
        let silences = video::detect_silences(&path_clone, -35.0, 0.6)?;
        Ok::<_, String>((probe, silences))
    })
    .await
    .map_err(|e| e.to_string())??;

    let suggested_segments = video::keep_segments(probe.duration_s, &silences, 0.15);

    let (transcript, transcript_error) = if req.skip_transcript.unwrap_or(false) || !probe.has_audio {
        (None, None)
    } else {
        match video::transcribe_video(&state, &path).await {
            Ok(t) => (Some(t), None),
            Err(e) => (None, Some(e)),
        }
    };

    let filler_ranges = transcript.as_ref().map(video::detect_filler_words).unwrap_or_default();
    let filler_keep_segments = if filler_ranges.is_empty() && silences.is_empty() {
        Vec::new()
    } else {
        video::combined_keep_segments(probe.duration_s, &silences, &filler_ranges, 0.15)
    };

    Ok(VideoAnalysis {
        probe,
        silences,
        suggested_segments,
        transcript,
        transcript_error,
        filler_ranges,
        filler_keep_segments,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoPlanRequest {
    pub path: String,
    pub instruction: String,
    /// Analysis results the frontend already has (avoids re-transcribing).
    pub transcript: Option<Transcript>,
    pub silences: Option<Vec<SilenceRange>>,
    /// Detected filler-word/stutter ranges from analysis (avoids re-scanning).
    pub filler_ranges: Option<Vec<SilenceRange>>,
    /// The plan as it currently stands in the editor (from a prior AI plan or manual edits).
    /// When present, the planner treats the instruction as a refinement: only the fields the
    /// instruction asks to change are updated, everything else is preserved verbatim.
    pub current_plan: Option<EditPlan>,
}

#[tauri::command]
pub async fn video_plan_edit(
    state: State<'_, AppState>,
    req: VideoPlanRequest,
) -> Result<EditPlan, String> {
    let path = PathBuf::from(&req.path);
    let probe = tauri::async_runtime::spawn_blocking(move || video::probe_video(&path))
        .await
        .map_err(|e| e.to_string())??;
    crate::llm::generate_video_edit_plan(
        &state,
        &req.instruction,
        &probe,
        req.transcript.as_ref(),
        req.silences.as_deref().unwrap_or(&[]),
        req.filler_ranges.as_deref().unwrap_or(&[]),
        req.current_plan.as_ref(),
    )
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoRenderRequest {
    pub path: String,
    pub plan: EditPlan,
    /// Correlates video-render-progress events with the requesting tile.
    pub job_id: String,
    /// Word-timestamped transcript for burned captions. If the plan asks for captions and
    /// none is supplied, the video is transcribed first.
    pub transcript: Option<Transcript>,
}

/// Render an edit into the Content library (so the gallery picks it up) and return the
/// output path. Long-running; progress arrives via `video-render-progress` events.
#[tauri::command]
pub async fn video_render(
    app: AppHandle,
    state: State<'_, AppState>,
    req: VideoRenderRequest,
) -> Result<String, String> {
    let src = PathBuf::from(&req.path);
    // Captions need a transcript; transcribe up front (off spawn_blocking) if not provided.
    let transcript = if req.plan.captions && req.transcript.is_none() {
        Some(video::transcribe_video(&state, &src).await.map_err(|e| {
            format!("captions need a transcript, but transcription failed: {e}")
        })?)
    } else {
        req.transcript
    };

    let prefer_hw_encode = video::hw_encode_preferred(&state);
    let out_dir = crate::tools::content_dir(&app)?;
    let app_clone = app.clone();
    let out = tauri::async_runtime::spawn_blocking(move || {
        video::render_edit(&app_clone, &req.job_id, &src, &req.plan, transcript.as_ref(), &out_dir, prefer_hw_encode)
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = app.emit("content-changed", ());
    Ok(out.to_string_lossy().to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoExportSubtitlesRequest {
    pub path: String,
    /// Existing transcript; if absent the video is transcribed.
    pub transcript: Option<Transcript>,
    /// "srt" or "vtt".
    pub format: String,
    /// Optional edit plan whose timeline mapping is applied to the caption times.
    pub plan: Option<EditPlan>,
}

/// Export the transcript as an SRT or VTT sidecar into the Content library, applying the
/// plan's timeline mapping when one is given. Returns the written file path.
#[tauri::command]
pub async fn video_export_subtitles(
    app: AppHandle,
    state: State<'_, AppState>,
    req: VideoExportSubtitlesRequest,
) -> Result<String, String> {
    let fmt = match req.format.as_str() {
        "srt" => "srt",
        "vtt" => "vtt",
        other => return Err(format!("unsupported subtitle format '{other}' (use \"srt\" or \"vtt\")")),
    };
    let src = PathBuf::from(&req.path);
    let src_probe = src.clone();
    let probe = tauri::async_runtime::spawn_blocking(move || video::probe_video(&src_probe))
        .await
        .map_err(|e| e.to_string())??;

    let transcript = match req.transcript {
        Some(t) => t,
        None => video::transcribe_video(&state, &src).await?,
    };

    let content = video::build_subtitles(&transcript, fmt, req.plan.as_ref(), &probe);
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("captions");
    let out_dir = crate::tools::content_dir(&app)?;
    let out_path = out_dir.join(format!("{stem}.{fmt}"));
    std::fs::write(&out_path, content).map_err(|e| e.to_string())?;
    let _ = app.emit("content-changed", ());
    Ok(out_path.to_string_lossy().to_string())
}

#[tauri::command]
pub fn video_cancel_render(job_id: String) -> bool {
    video::cancel_render(&job_id)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoShortsRequest {
    pub path: String,
    /// How many shorts to produce (default 3, max 8).
    pub count: Option<u32>,
    /// Target seconds per clip (default ~45; overrides the 30–60s default range).
    pub duration_hint_s: Option<f64>,
    /// Optional focus, e.g. "focus on the pricing discussion".
    pub brief: Option<String>,
    /// Correlates `shorts-progress` (and per-short `video-render-progress`) events + cancellation.
    pub job_id: String,
    /// Burn karaoke captions into each short (default true).
    pub captions: Option<bool>,
    /// Skip the normalize-audio + fade polish pass.
    pub skip_polish: Option<bool>,
}

/// Auto-shorts: transcribe a recording, have the LLM pick the best highlight moments, and render
/// each as a captioned 9:16 vertical clip into the Content library. Long-running; emits
/// `shorts-progress` for the pipeline stage and `video-render-progress` (keyed `{job_id}-{n}`)
/// for the active short. Cancel with `video_cancel_shorts`.
#[tauri::command]
pub async fn video_generate_shorts(
    app: AppHandle,
    state: State<'_, AppState>,
    req: VideoShortsRequest,
) -> Result<Vec<video::ShortResult>, String> {
    let src = PathBuf::from(&req.path);
    video::run_shorts_pipeline(
        &app,
        &state,
        &src,
        req.count.unwrap_or(3),
        req.duration_hint_s,
        req.brief.as_deref(),
        &req.job_id,
        req.captions.unwrap_or(true),
        req.skip_polish.unwrap_or(false),
    )
    .await
}

#[tauri::command]
pub fn video_cancel_shorts(job_id: String) -> bool {
    video::request_cancel_shorts(&job_id)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSavePlanRequest {
    pub path: String,
    pub plan: EditPlan,
}

/// Persist the in-progress edit plan for a source video, keyed by its absolute path, so
/// reopening the same file restores where the user left off. Upserts `video_plans`.
#[tauri::command]
pub fn video_save_plan(state: State<'_, AppState>, req: VideoSavePlanRequest) -> Result<(), String> {
    let plan_json = serde_json::to_string(&req.plan).map_err(|e| e.to_string())?;
    with_db(&state, |db| {
        db.execute(
            "INSERT INTO video_plans (path, plan_json, updated_at) VALUES (?1, ?2, datetime('now')) \
             ON CONFLICT(path) DO UPDATE SET plan_json = excluded.plan_json, updated_at = excluded.updated_at",
            params![req.path, plan_json],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoLoadPlanRequest {
    pub path: String,
}

/// Load a previously saved edit plan for a source video, if any. Returns `None` (not an
/// error) both when nothing was saved yet and when the stored JSON no longer deserializes
/// (e.g. after an `EditPlan` shape change) — a corrupt/stale save shouldn't block opening
/// the file.
#[tauri::command]
pub fn video_load_plan(state: State<'_, AppState>, req: VideoLoadPlanRequest) -> Result<Option<EditPlan>, String> {
    with_db(&state, |db| {
        let row: Option<String> = db
            .query_row(
                "SELECT plan_json FROM video_plans WHERE path = ?1",
                params![req.path],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row.and_then(|s| serde_json::from_str::<EditPlan>(&s).ok()))
    })
}
