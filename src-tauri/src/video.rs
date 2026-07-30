//! Video engine: ffmpeg/ffprobe wrappers powering the Studio Video tab and the agent
//! video tools (probe / analyze / edit). Everything runs locally via the user's ffmpeg
//! install (Homebrew). Renders are non-destructive: the source file is never modified,
//! output always goes to a new file.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::db::get_app_setting_value;
use crate::AppState;

// ---------- binary discovery ----------

/// GUI apps don't inherit the shell PATH, so check the common install prefixes first.
fn find_binary(name: &str) -> Option<PathBuf> {
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let p = Path::new(dir).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub(crate) fn ffmpeg_path() -> Result<PathBuf, String> {
    find_binary("ffmpeg")
        .ok_or_else(|| "ffmpeg is not installed. Install it with: brew install ffmpeg".to_string())
}

pub(crate) fn ffprobe_path() -> Result<PathBuf, String> {
    find_binary("ffprobe")
        .ok_or_else(|| "ffprobe is not installed. Install it with: brew install ffmpeg".to_string())
}

fn font_path() -> Option<&'static str> {
    [
        "/System/Library/Fonts/Helvetica.ttc",
        "/System/Library/Fonts/HelveticaNeue.ttc",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ]
    .into_iter()
    .find(|p| Path::new(p).is_file())
}

// ---------- probe ----------

#[derive(Serialize, Clone, Debug)]
pub struct VideoProbe {
    pub path: String,
    pub duration_s: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
    pub video_codec: String,
    pub size_bytes: u64,
}

pub(crate) fn probe_video(path: &Path) -> Result<VideoProbe, String> {
    if !path.is_file() {
        return Err(format!("file not found: {}", path.display()));
    }
    let out = Command::new(ffprobe_path()?)
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffprobe error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe parse error: {e}"))?;
    let streams = v.get("streams").and_then(Value::as_array).cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
        .ok_or_else(|| "no video stream found in file".to_string())?;
    let has_audio = streams
        .iter()
        .any(|s| s.get("codec_type").and_then(Value::as_str) == Some("audio"));

    let fps = video
        .get("avg_frame_rate")
        .and_then(Value::as_str)
        .and_then(|r| {
            let mut it = r.split('/');
            let num: f64 = it.next()?.parse().ok()?;
            let den: f64 = it.next().unwrap_or("1").parse().ok()?;
            if den > 0.0 { Some(num / den) } else { None }
        })
        .unwrap_or(0.0);

    Ok(VideoProbe {
        path: path.to_string_lossy().to_string(),
        duration_s: v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0),
        width: video.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
        height: video.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
        fps: (fps * 100.0).round() / 100.0,
        has_audio,
        video_codec: video.get("codec_name").and_then(Value::as_str).unwrap_or("").to_string(),
        size_bytes: v["format"]["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
    })
}

// ---------- silence detection ----------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SilenceRange {
    pub start: f64,
    pub end: f64,
}

pub(crate) fn detect_silences(path: &Path, noise_db: f64, min_silence_s: f64) -> Result<Vec<SilenceRange>, String> {
    let probe = probe_video(path)?;
    if !probe.has_audio {
        return Ok(Vec::new());
    }
    let out = Command::new(ffmpeg_path()?)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-af",
            &format!("silencedetect=noise={noise_db}dB:d={min_silence_s}"),
            "-f",
            "null",
            "-",
        ])
        .output()
        .map_err(|e| format!("ffmpeg failed to start: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    Ok(parse_silences(&stderr, probe.duration_s))
}

/// Parse `silencedetect` stderr lines, pairing starts with ends (an unclosed start runs
/// to the end of the video).
fn parse_silences(stderr: &str, duration_s: f64) -> Vec<SilenceRange> {
    let mut ranges = Vec::new();
    let mut open_start: Option<f64> = None;
    for line in stderr.lines() {
        if let Some(idx) = line.find("silence_start:") {
            open_start = line[idx + "silence_start:".len()..].trim().parse().ok();
        } else if let Some(idx) = line.find("silence_end:") {
            let rest = &line[idx + "silence_end:".len()..];
            let end: Option<f64> = rest.split('|').next().and_then(|s| s.trim().parse().ok());
            if let (Some(start), Some(end)) = (open_start.take(), end) {
                ranges.push(SilenceRange { start, end });
            }
        }
    }
    if let Some(start) = open_start {
        if duration_s > start {
            ranges.push(SilenceRange { start, end: duration_s });
        }
    }
    ranges
}

/// Invert silence ranges into the segments worth keeping, padding each cut so speech
/// isn't clipped mid-word. Silences shorter than the detector threshold never appear
/// here, so everything between detected silences is kept.
pub(crate) fn keep_segments(duration_s: f64, silences: &[SilenceRange], pad_s: f64) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut cursor = 0.0_f64;
    for s in silences {
        let end = (s.start + pad_s).min(duration_s);
        if end - cursor > 0.05 {
            segments.push(Segment::keep(cursor, end));
        }
        cursor = (s.end - pad_s).max(cursor);
    }
    if duration_s - cursor > 0.05 {
        segments.push(Segment::keep(cursor, duration_s));
    }
    segments
}

// ---------- filler-word detection ----------

/// Case-insensitive, punctuation-stripped filler words we cut unambiguously. Deliberately
/// conservative: "like" and "you know" are skipped entirely since they're too often
/// semantically load-bearing to cut blindly from a word-level heuristic.
const FILLER_WORDS: &[&str] = &["um", "uh", "uhm", "umm", "er", "erm", "hmm", "mhm"];

/// A repeated word ("I I", "the the") counts as a stutter when the gap between the two
/// occurrences is under this many seconds.
const STUTTER_GAP_S: f64 = 0.5;

/// Cut ranges within this many seconds of each other are merged into one.
const MERGE_GAP_S: f64 = 0.15;

/// Lowercase a word and strip everything but letters/digits, so "Um," "uh-" and "Uh!"
/// all normalize to their bare filler form.
fn normalize_word(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// Sort ranges by start and merge any that overlap or sit within `gap_s` of each other.
fn merge_ranges(mut ranges: Vec<SilenceRange>, gap_s: f64) -> Vec<SilenceRange> {
    ranges.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<SilenceRange> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.start - last.end < gap_s {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }
    merged
}

/// Detect unambiguous filler words ("um", "uh", ...) and immediate word-repeat stutters
/// ("I I", "the the" — same normalized word twice within `STUTTER_GAP_S`, cutting the
/// first occurrence) from a transcript's word timings. Returns merged cut ranges on the
/// SOURCE timeline. Pure/unit-tested; returns empty when the transcript has no word
/// timings.
pub(crate) fn detect_filler_words(transcript: &Transcript) -> Vec<SilenceRange> {
    let words = &transcript.words;
    let mut cuts: Vec<SilenceRange> = Vec::new();

    for w in words {
        if FILLER_WORDS.contains(&normalize_word(&w.word).as_str()) {
            cuts.push(SilenceRange { start: w.start, end: w.end });
        }
    }
    for pair in words.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let na = normalize_word(&a.word);
        if !na.is_empty() && na == normalize_word(&b.word) && (b.start - a.end) < STUTTER_GAP_S {
            cuts.push(SilenceRange { start: a.start, end: a.end });
        }
    }
    merge_ranges(cuts, MERGE_GAP_S)
}

/// Merge filler-word cut ranges with detected silence ranges and invert the result into
/// keep-segments via `keep_segments`, so one call yields the segments for "remove
/// silences + fillers". Resulting keep-segments shorter than `MERGE_GAP_S` (a sliver left
/// between two nearby cuts) are dropped as not worth keeping.
pub(crate) fn combined_keep_segments(
    duration_s: f64,
    silences: &[SilenceRange],
    fillers: &[SilenceRange],
    pad_s: f64,
) -> Vec<Segment> {
    let mut cuts = Vec::with_capacity(silences.len() + fillers.len());
    cuts.extend(silences.iter().cloned());
    cuts.extend(fillers.iter().cloned());
    let merged = merge_ranges(cuts, MERGE_GAP_S);
    keep_segments(duration_s, &merged, pad_s)
        .into_iter()
        .filter(|s| s.end - s.start >= MERGE_GAP_S)
        .collect()
}

// ---------- filter presets ----------

/// (key, label, ffmpeg video filter chain). The chains are plain `-vf`-style strings
/// appended after the segment concat; keep them free of commas that would need escaping.
pub(crate) const FILTER_PRESETS: &[(&str, &str, &str)] = &[
    ("none", "None", ""),
    (
        "cinematic",
        "Cinematic",
        "eq=brightness=-0.02:saturation=0.92:contrast=1.12,colorbalance=bs=0.05:bh=-0.05:rh=0.03",
    ),
    ("warm", "Warm", "colortemperature=temperature=4800,eq=saturation=1.1:contrast=1.03"),
    ("cool", "Cool", "colortemperature=temperature=8500,eq=saturation=1.05:contrast=1.05"),
    ("vintage", "Vintage", "curves=preset=vintage,vignette=PI/5,eq=saturation=0.85"),
    ("bw", "Black & White", "hue=s=0,eq=contrast=1.12"),
    ("punchy", "Punchy", "eq=brightness=0.03:saturation=1.35:contrast=1.1,unsharp=5:5:0.6"),
];

pub(crate) fn preset_chain(key: &str) -> Result<&'static str, String> {
    FILTER_PRESETS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, _, chain)| *chain)
        .ok_or_else(|| {
            format!(
                "unknown filter preset '{key}'. Available: {}",
                FILTER_PRESETS.iter().map(|(k, _, _)| *k).collect::<Vec<_>>().join(", ")
            )
        })
}

// ---------- encoder selection ----------

/// Video encoder choice for a render. `H264Videotoolbox` is Apple's hardware encoder —
/// much faster and cooler-running than software x264 — but stock ffmpeg builds don't
/// expose a CRF/constant-quality mode for it, so it's driven with an explicit bitrate
/// instead (see `videotoolbox_bitrate_bps`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoEncoder {
    Libx264,
    H264Videotoolbox,
}

/// Whether the ffmpeg binary on disk was built with `h264_videotoolbox` support. Checked
/// once per process — spawning `ffmpeg -encoders` on every render would be wasteful — and
/// cached in a `OnceLock`.
fn has_videotoolbox_encoder() -> bool {
    static HAS_VT: OnceLock<bool> = OnceLock::new();
    *HAS_VT.get_or_init(|| {
        ffmpeg_path()
            .ok()
            .and_then(|ffmpeg| Command::new(ffmpeg).args(["-hide_banner", "-encoders"]).output().ok())
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("h264_videotoolbox"))
            .unwrap_or(false)
    })
}

/// Pick the encoder for a render: hardware VideoToolbox when the caller opted in, we're on
/// macOS, and the ffmpeg build actually has it available; software libx264 otherwise.
pub(crate) fn pick_encoder(prefer_hw: bool) -> VideoEncoder {
    if prefer_hw && cfg!(target_os = "macos") && has_videotoolbox_encoder() {
        VideoEncoder::H264Videotoolbox
    } else {
        VideoEncoder::Libx264
    }
}

/// VideoToolbox has no CRF-equivalent constant-quality mode on stock ffmpeg builds, so it's
/// driven with an explicit bitrate sized off the output resolution and frame rate (roughly
/// 0.1-0.15 bits/pixel/frame), clamped to a sane 2-12 Mbps range so tiny or huge outputs
/// don't get a silly bitrate.
pub(crate) fn videotoolbox_bitrate_bps(width: u32, height: u32, fps: f64) -> u64 {
    const BITS_PER_PIXEL_PER_FRAME: f64 = 0.12;
    const MIN_BPS: f64 = 2_000_000.0;
    const MAX_BPS: f64 = 12_000_000.0;
    let fps = if fps > 1.0 { fps } else { 30.0 };
    let raw = width as f64 * height as f64 * fps * BITS_PER_PIXEL_PER_FRAME;
    raw.clamp(MIN_BPS, MAX_BPS) as u64
}

/// Whether the `video/hw_encode` setting prefers hardware encoding — default ON when
/// unset. DB errors are treated as "unset" so a settings-read hiccup never blocks a render.
pub(crate) fn hw_encode_preferred(state: &AppState) -> bool {
    get_app_setting_value(state, "video/hw_encode")
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Whether an ffmpeg exit status indicates the process was killed by a signal — as
/// `cancel_render` does with `kill -9` — rather than failing on its own. A cancelled render
/// must never trigger the hardware-encoder fallback retry in `render_edit`, since a fast
/// cancel looks identical to a fast encoder-init failure from the exit code alone.
fn was_signal_killed(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal().is_some()
}

// ---------- edit plan ----------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    /// Which source the `start`..`end` range comes from. `None` (the serde default) means
    /// the MAIN video being edited. `Some(path)` makes this an *insert*: a b-roll clip from
    /// another video file (trim `start`..`end` from it), or a still image (png/jpg/jpeg/webp)
    /// shown for `end - start` seconds as a silent title/intro card. Insert `start`/`end` are
    /// on the insert's OWN timeline, not the main video's (images conventionally use start=0).
    #[serde(default)]
    pub source: Option<String>,
}

impl Segment {
    /// A keep-range on the MAIN source (no separate insert file).
    pub(crate) fn keep(start: f64, end: f64) -> Self {
        Segment { start, end, source: None }
    }
}

/// Whether a source path points at a still image we should show as a card (by extension —
/// so it works in `validate()` without touching the filesystem). Case-insensitive.
pub(crate) fn is_image_source(path: &str) -> bool {
    let lower = path.trim().to_lowercase();
    [".png", ".jpg", ".jpeg", ".webp"].iter().any(|ext| lower.ends_with(ext))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TextOverlay {
    pub text: String,
    /// Seconds on the OUTPUT (post-cut, post-speed) timeline.
    pub start: f64,
    pub end: f64,
    /// "top" | "center" | "bottom" (default bottom).
    #[serde(default)]
    pub position: Option<String>,
    #[serde(default)]
    pub size: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct EditPlan {
    /// Source-timeline ranges to keep, in order. Empty = keep the whole video.
    pub segments: Vec<Segment>,
    /// Filter preset key (see FILTER_PRESETS). None/"none" = untouched.
    pub filter: Option<String>,
    /// Text overlays, timed on the output timeline.
    pub texts: Vec<TextOverlay>,
    /// Target aspect like "9:16", "1:1", "4:5" — center-crops. None = keep as-is.
    pub aspect: Option<String>,
    /// Playback speed multiplier (0.25–4.0). None = 1.0.
    pub speed: Option<f64>,
    /// Drop the audio track entirely.
    pub mute: bool,
    /// Optional output file stem (sanitized); a timestamp suffix is always added.
    pub output_name: Option<String>,
    /// Burn word-timed subtitles from the transcript into the video.
    #[serde(default)]
    pub captions: bool,
    /// Caption style preset (see CAPTION_STYLES). None = "boxed".
    #[serde(default)]
    pub caption_style: Option<String>,
    /// Loudness-normalize the final mix (EBU R128, -14 LUFS/-1.5dB TP/11 LRA) — applied as
    /// the LAST audio filter, after atempo and any overall fade. Good for publishable audio.
    #[serde(default)]
    pub normalize_audio: bool,
    /// Reduce background hiss/hum (afftdn) on each kept audio segment, before atempo, the
    /// overall fade, and loudnorm. Good for noisy recordings.
    #[serde(default)]
    pub denoise: bool,
    /// Overall fade-in/out duration in seconds (0.1–5.0), applied to both video and audio
    /// at the very start/end of the OUTPUT timeline. None = no fade.
    #[serde(default)]
    pub fade: Option<f64>,
    /// Subject-aware crop: when `aspect` makes the output NARROWER than the source (e.g.
    /// 16:9 → 9:16), detect faces on-device and drive a dynamic horizontal crop so speakers
    /// stay in frame instead of being decapitated by a static center-crop. A harmless no-op
    /// when the aspect doesn't narrow, when no faces are detected, or when the framescan
    /// sidecar is unavailable (all fall back silently to the center crop).
    #[serde(default)]
    pub smart_reframe: bool,
}

/// Available burned-in caption styles: "boxed" (white on a semi-transparent black box —
/// the default), "outline" (white with a heavy black outline), and "karaoke" (word-by-word
/// highlight driven by ASS `\k` tags).
pub(crate) const CAPTION_STYLES: &[&str] = &["boxed", "outline", "karaoke"];

impl EditPlan {
    /// Validate against the probed source and normalize (clamp/sort) in place.
    pub(crate) fn validate(&mut self, probe: &VideoProbe) -> Result<(), String> {
        if self.segments.len() > 200 {
            return Err("too many segments (max 200)".to_string());
        }
        if self.texts.len() > 20 {
            return Err("too many text overlays (max 20)".to_string());
        }
        for s in &mut self.segments {
            match &s.source {
                // MAIN-source keep-range: clamp against the (probed) main duration.
                None => {
                    s.start = s.start.max(0.0);
                    s.end = s.end.min(probe.duration_s + 0.5);
                    if s.end - s.start < 0.05 {
                        return Err(format!("segment {:.2}–{:.2}s is empty or inverted", s.start, s.end));
                    }
                }
                // Insert segment: its start/end are on the INSERT's own timeline, which we
                // can't probe here (no fs in validate). Do only static checks — existence and
                // per-file probing happen in `render_edit`.
                Some(src) => {
                    let src = src.trim();
                    if src.is_empty() {
                        return Err("segment has an empty source path".to_string());
                    }
                    if s.end - s.start < 0.05 {
                        return Err(format!(
                            "insert segment '{src}' has an empty or inverted range ({:.2}–{:.2}s)",
                            s.start, s.end
                        ));
                    }
                    if is_image_source(src) {
                        let dur = s.end - s.start;
                        if !(0.2..=30.0).contains(&dur) {
                            return Err(format!(
                                "image insert '{src}' duration must be 0.2–30s (got {dur:.2}s)"
                            ));
                        }
                    }
                    s.start = s.start.max(0.0);
                }
            }
        }
        if let Some(sp) = self.speed {
            if !(0.25..=4.0).contains(&sp) {
                return Err("speed must be between 0.25 and 4.0".to_string());
            }
        }
        if let Some(f) = self.filter.as_deref() {
            preset_chain(f)?; // errors on unknown key
        }
        if let Some(a) = self.aspect.as_deref() {
            parse_aspect(a)?;
        }
        if let Some(cs) = self.caption_style.as_deref() {
            if !CAPTION_STYLES.contains(&cs) {
                return Err(format!(
                    "unknown caption style '{cs}'. Available: {}",
                    CAPTION_STYLES.join(", ")
                ));
            }
        }
        if let Some(f) = self.fade {
            if !(0.1..=5.0).contains(&f) {
                return Err("fade must be between 0.1 and 5.0 seconds".to_string());
            }
        }
        for t in &self.texts {
            if t.text.trim().is_empty() {
                return Err("text overlay with empty text".to_string());
            }
            if t.end <= t.start {
                return Err(format!("text overlay '{}' has end <= start", t.text));
            }
        }
        Ok(())
    }

    /// Expected output duration in seconds (drives the progress percentage).
    pub(crate) fn output_duration(&self, probe: &VideoProbe) -> f64 {
        let base: f64 = if self.segments.is_empty() {
            probe.duration_s
        } else {
            self.segments.iter().map(|s| s.end - s.start).sum()
        };
        base / self.speed.unwrap_or(1.0)
    }
}

fn parse_aspect(a: &str) -> Result<(u32, u32), String> {
    let mut it = a.split(':');
    let w: u32 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let h: u32 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if w == 0 || h == 0 || w > 32 || h > 32 {
        return Err(format!("invalid aspect '{a}' — use e.g. \"9:16\", \"1:1\", \"4:5\""));
    }
    Ok((w, h))
}

/// atempo only accepts 0.5–2.0 per instance; chain instances for larger factors.
fn atempo_chain(speed: f64) -> String {
    let mut parts = Vec::new();
    let mut remaining = speed;
    while remaining > 2.0 {
        parts.push("atempo=2.0".to_string());
        remaining /= 2.0;
    }
    while remaining < 0.5 {
        parts.push("atempo=0.5".to_string());
        remaining /= 0.5;
    }
    parts.push(format!("atempo={remaining:.6}"));
    parts.join(",")
}

fn drawtext_position(position: Option<&str>) -> &'static str {
    match position.unwrap_or("bottom") {
        "top" => "x=(w-text_w)/2:y=h*0.08",
        "center" => "x=(w-text_w)/2:y=(h-text_h)/2",
        _ => "x=(w-text_w)/2:y=h-text_h-h*0.08",
    }
}

// ---------- caption timeline mapping + ASS generation ----------

/// The kept segments the render actually uses (empty plan = the whole source).
fn effective_segments(plan: &EditPlan, probe: &VideoProbe) -> Vec<Segment> {
    if plan.segments.is_empty() {
        vec![Segment::keep(0.0, probe.duration_s)]
    } else {
        plan.segments.clone()
    }
}

/// Cumulative pre-speed OUTPUT offset at the start of each kept segment.
fn segment_offsets(segments: &[Segment]) -> Vec<f64> {
    let mut offs = Vec::with_capacity(segments.len());
    let mut acc = 0.0_f64;
    for s in segments {
        offs.push(acc);
        acc += (s.end - s.start).max(0.0);
    }
    offs
}

/// Whether two source references denote the same render input: both `None` = the MAIN video,
/// or a trimmed-path match. Insert sources are matched by the same trimmed-path string the
/// renderer uses in `seg_input`, so a caption's source key lines up with its segments.
fn source_key_eq(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.trim() == y.trim(),
        _ => false,
    }
}

/// Transcripts for a (possibly multi-source) render, keyed by the source they describe. The
/// `None` key is the MAIN video (`inputs[0]`); a `Some(path)` key matches a `Segment.source`
/// by the same trimmed-path string the renderer resolves inputs with. A single-source render
/// carries just one `None` entry, and that path produces byte-identical captions to the
/// pre-multi-source behavior (see `build_ass`/`build_subtitles`).
#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptMap {
    /// (source_key, transcript) pairs. `source_key == None` is the MAIN source.
    entries: Vec<(Option<String>, Transcript)>,
}

impl TranscriptMap {
    /// The MAIN video's transcript only — the common single-source case.
    pub(crate) fn single(transcript: Transcript) -> Self {
        TranscriptMap { entries: vec![(None, transcript)] }
    }

    /// Build from the `Option<&Transcript>` the existing single-source callers hold. `None`
    /// yields an empty map (no captions), matching the old "no transcript" behavior.
    pub(crate) fn from_single(transcript: Option<&Transcript>) -> Self {
        match transcript {
            Some(t) => TranscriptMap::single(t.clone()),
            None => TranscriptMap::default(),
        }
    }

    /// Add a source's transcript. `source == None` is the MAIN source; a `Some(path)` key is
    /// trimmed so it matches `Segment.source` the way the renderer resolves inputs.
    pub(crate) fn insert(&mut self, source: Option<&str>, transcript: Transcript) {
        let key = source.map(|s| s.trim().to_string());
        self.entries.push((key, transcript));
    }

    /// Whether any source carries words/segments worth burning captions for.
    pub(crate) fn has_captions(&self) -> bool {
        self.entries.iter().any(|(_, t)| !(t.words.is_empty() && t.segments.is_empty()))
    }
}

/// Map a SOURCE-timeline interval from `word_source` to the OUTPUT timeline for the given kept
/// segments and speed. Returns `(out_start, out_end, segment_index)`. Intervals that fall
/// entirely outside every kept segment of that source return `None` (the word is dropped);
/// partially-clipped intervals are clamped to the segment they most overlap.
///
/// A word only maps into segments whose `source` matches `word_source` — a segment from
/// another source lives on a different timeline and must never "capture" this word. ALL
/// segments (of every source) still count toward the cumulative output offset (via
/// `segment_offsets`), so a word is pushed later on the output timeline by the duration of
/// every earlier segment, whichever source it came from. With `word_source == &None` this is
/// exactly the old main-source-only mapping.
fn map_interval_to_output_for_source(
    ws: f64,
    we: f64,
    word_source: &Option<String>,
    segments: &[Segment],
    speed: f64,
) -> Option<(f64, f64, usize)> {
    let speed = if speed > 0.0 { speed } else { 1.0 };
    let offs = segment_offsets(segments);
    let mut best: Option<(usize, f64)> = None;
    for (i, s) in segments.iter().enumerate() {
        if !source_key_eq(&s.source, word_source) {
            continue; // segment belongs to another source — this word never maps into it
        }
        let overlap = we.min(s.end) - ws.max(s.start);
        if overlap > 0.0 && best.map_or(true, |(_, b)| overlap > b) {
            best = Some((i, overlap));
        }
    }
    let (i, _) = best?;
    let s = &segments[i];
    let cs = ws.max(s.start);
    let ce = we.min(s.end);
    let out_start = (offs[i] + (cs - s.start)) / speed;
    let out_end = (offs[i] + (ce - s.start)) / speed;
    Some((out_start, out_end, i))
}

/// Main-source (`source == None`) interval mapping — the common single-transcript path.
#[cfg_attr(not(test), allow(dead_code))]
fn map_interval_to_output(
    ws: f64,
    we: f64,
    segments: &[Segment],
    speed: f64,
) -> Option<(f64, f64, usize)> {
    map_interval_to_output_for_source(ws, we, &None, segments, speed)
}

/// OUTPUT resolution after an optional center-crop, floored to even pixels (mirrors the
/// crop+scale stages in `build_render_args`). Drives PlayResX/Y so ASS styling scales.
fn output_resolution(plan: &EditPlan, probe: &VideoProbe) -> (u32, u32) {
    let floor_even = |v: f64| -> u32 {
        let n = v.max(2.0) as u32;
        n - (n % 2)
    };
    if let Some(a) = plan.aspect.as_deref() {
        if let Ok((aw, ah)) = parse_aspect(a) {
            let (iw, ih) = (probe.width as f64, probe.height as f64);
            let cw = iw.min(ih * aw as f64 / ah as f64);
            let ch = ih.min(iw * ah as f64 / aw as f64);
            return (floor_even(cw), floor_even(ch));
        }
    }
    (probe.width.max(2), probe.height.max(2))
}

#[derive(Clone, Debug)]
struct CapWord {
    out_start: f64,
    out_end: f64,
    text: String,
}

#[derive(Debug)]
struct CaptionLine {
    start: f64,
    end: f64,
    words: Vec<CapWord>,
}

impl CaptionLine {
    fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }
}

/// Group ONE source's transcript into short caption lines (~3–5 words / ~28 chars), breaking
/// at speech gaps > 0.8s, transcript-segment boundaries, or edit cuts, and mapping each word
/// onto the OUTPUT timeline via that source's kept segments (`word_source`). Falls back to
/// segment-level lines when the transcript has no word timings. Words that don't land inside
/// any of `word_source`'s kept segments are dropped. A line never spans two segments/sources
/// because every kept-segment change flushes the current line (`eseg != prev_eseg`).
fn group_caption_lines_for_source(
    transcript: &Transcript,
    word_source: &Option<String>,
    segments: &[Segment],
    speed: f64,
) -> Vec<CaptionLine> {
    const MAX_WORDS: usize = 5;
    const MAX_CHARS: usize = 28;
    const GAP_S: f64 = 0.8;

    struct Src {
        start: f64,
        end: f64,
        text: String,
        tseg: usize,
    }
    let mut items: Vec<Src> = Vec::new();
    if !transcript.words.is_empty() {
        for w in &transcript.words {
            let tseg = transcript
                .segments
                .iter()
                .position(|s| w.start >= s.start && w.start < s.end)
                .unwrap_or(usize::MAX);
            items.push(Src { start: w.start, end: w.end, text: w.word.clone(), tseg });
        }
    } else {
        for (i, s) in transcript.segments.iter().enumerate() {
            items.push(Src { start: s.start, end: s.end, text: s.text.clone(), tseg: i });
        }
    }

    let mut lines: Vec<CaptionLine> = Vec::new();
    let mut cur: Vec<CapWord> = Vec::new();
    let mut cur_chars = 0usize;
    let mut prev_src_end = f64::NEG_INFINITY;
    let mut prev_tseg = usize::MAX;
    let mut prev_eseg = usize::MAX;

    let flush = |lines: &mut Vec<CaptionLine>, cur: &mut Vec<CapWord>| {
        if cur.is_empty() {
            return;
        }
        let start = cur.iter().map(|w| w.out_start).fold(f64::INFINITY, f64::min);
        let end = cur.iter().map(|w| w.out_end).fold(f64::NEG_INFINITY, f64::max);
        lines.push(CaptionLine { start, end, words: std::mem::take(cur) });
    };

    for it in &items {
        let (os, oe, eseg) =
            match map_interval_to_output_for_source(it.start, it.end, word_source, segments, speed) {
                Some(v) => v,
                None => continue,
            };
        let wlen = it.text.chars().count().max(1);
        let gap = it.start - prev_src_end;
        let boundary = !cur.is_empty()
            && (cur.len() >= MAX_WORDS
                || cur_chars + 1 + wlen > MAX_CHARS
                || gap > GAP_S
                || it.tseg != prev_tseg
                || eseg != prev_eseg);
        if boundary {
            flush(&mut lines, &mut cur);
            cur_chars = 0;
        }
        cur_chars += if cur.is_empty() { wlen } else { wlen + 1 };
        cur.push(CapWord { out_start: os, out_end: oe, text: it.text.clone() });
        prev_src_end = it.end;
        prev_tseg = it.tseg;
        prev_eseg = eseg;
    }
    flush(&mut lines, &mut cur);
    lines
}

/// Main-source (`source == None`) caption grouping — the single-transcript path.
#[cfg_attr(not(test), allow(dead_code))]
fn group_caption_lines(transcript: &Transcript, segments: &[Segment], speed: f64) -> Vec<CaptionLine> {
    group_caption_lines_for_source(transcript, &None, segments, speed)
}

/// Group EVERY source's transcript into caption lines and merge them onto one output timeline.
/// Each source is grouped independently against its own kept segments (so a line never mixes
/// words from different sources), then all lines are sorted by output start time. A source with
/// no transcript, or whose segments were all cut, simply contributes no lines. With a single
/// `None`-keyed transcript this is identical to `group_caption_lines` (one pass + a stable
/// sort over already-ordered lines).
fn group_caption_lines_multi(transcripts: &TranscriptMap, segments: &[Segment], speed: f64) -> Vec<CaptionLine> {
    let mut lines: Vec<CaptionLine> = Vec::new();
    for (src_key, transcript) in &transcripts.entries {
        lines.extend(group_caption_lines_for_source(transcript, src_key, segments, speed));
    }
    lines.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    lines
}

/// Format seconds as an ASS timestamp `H:MM:SS.cc` (centiseconds).
fn ass_time(t: f64) -> String {
    let cs = (t.max(0.0) * 100.0).round() as i64;
    format!("{}:{:02}:{:02}.{:02}", cs / 360_000, (cs % 360_000) / 6_000, (cs % 6_000) / 100, cs % 100)
}

/// Escape a caption string for the ASS Dialogue text field: newlines become `\N` and the
/// override-tag braces are neutralized so user text can't inject styling.
fn ass_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('{', "(").replace('}', ")").replace('\n', "\\N")
}

/// Single-source convenience wrapper for `build_ass_multi` (used by the studio single-video
/// render + tests). Behaves EXACTLY as before multi-source captions existed.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_ass(transcript: &Transcript, plan: &EditPlan, probe: &VideoProbe) -> String {
    build_ass_multi(&TranscriptMap::single(transcript.clone()), plan, probe)
}

/// Build a full ASS subtitle document for the plan's captions from a per-source transcript
/// map. Pure/unit-testable. Times are on the OUTPUT timeline and PlayResX/Y match the OUTPUT
/// resolution so styling scales. Words from each source map only into that source's kept
/// segments (see `group_caption_lines_multi`).
pub(crate) fn build_ass_multi(transcripts: &TranscriptMap, plan: &EditPlan, probe: &VideoProbe) -> String {
    let (w, h) = output_resolution(plan, probe);
    let fs = (h as f64 / 12.0).round().max(12.0) as i64;
    let margin_v = (h as f64 * 0.08).round().max(10.0) as i64;
    let outline = (fs as f64 / 8.0).round().max(1.0) as i64;
    let style = plan.caption_style.as_deref().unwrap_or("boxed");
    let karaoke = style == "karaoke";

    // Format: Name,Fontname,Fontsize,PrimaryColour,SecondaryColour,OutlineColour,BackColour,
    //   Bold,Italic,Underline,StrikeOut,ScaleX,ScaleY,Spacing,Angle,BorderStyle,Outline,
    //   Shadow,Alignment,MarginL,MarginR,MarginV,Encoding. Colours are &HAABBGGRR.
    let style_line = match style {
        // Heavy black outline, no box.
        "outline" => format!(
            "Style: Default,Arial,{fs},&H00FFFFFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,{o},0,2,60,60,{mv},1",
            fs = fs, o = outline.max(3), mv = margin_v
        ),
        // Primary (highlighted/sung) = yellow, Secondary (not yet reached) = white.
        "karaoke" => format!(
            "Style: Default,Arial,{fs},&H0000FFFF,&H00FFFFFF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,{o},0,2,60,60,{mv},1",
            fs = fs, o = outline.max(3), mv = margin_v
        ),
        // "boxed" (default): opaque box (BorderStyle 3) in semi-transparent black.
        _ => format!(
            "Style: Default,Arial,{fs},&H00FFFFFF,&H000000FF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,3,{o},0,2,60,60,{mv},1",
            fs = fs, o = outline.max(6), mv = margin_v
        ),
    };

    let mut doc = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {w}\nPlayResY: {h}\nWrapStyle: 2\nScaledBorderAndShadow: yes\n\n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         {style_line}\n\n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    );

    let lines = group_caption_lines_multi(transcripts, &effective_segments(plan, probe), plan.speed.unwrap_or(1.0));
    for line in &lines {
        let text = if karaoke {
            let mut t = String::new();
            for (i, wd) in line.words.iter().enumerate() {
                // Highlight advances at each word onset; the last word runs to its end.
                let dur = if i + 1 < line.words.len() {
                    line.words[i + 1].out_start - wd.out_start
                } else {
                    wd.out_end - wd.out_start
                };
                let cs = ((dur * 100.0).round() as i64).max(1);
                t.push_str(&format!("{{\\k{cs}}}{} ", ass_escape(&wd.text)));
            }
            t.trim_end().to_string()
        } else {
            ass_escape(&line.text())
        };
        doc.push_str(&format!(
            "Dialogue: 0,{},{},Default,,0,0,0,,{}\n",
            ass_time(line.start),
            ass_time(line.end),
            text
        ));
    }
    doc
}

/// Format seconds as an SRT (`HH:MM:SS,mmm`) or VTT (`HH:MM:SS.mmm`) timestamp.
fn subtitle_time(t: f64, vtt: bool) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as i64;
    let sep = if vtt { '.' } else { ',' };
    format!("{:02}:{:02}:{:02}{}{:03}", ms / 3_600_000, (ms % 3_600_000) / 60_000, (ms % 60_000) / 1000, sep, ms % 1000)
}

/// Single-source SRT/VTT export — convenience wrapper for `build_subtitles_multi`.
pub(crate) fn build_subtitles(
    transcript: &Transcript,
    format: &str,
    plan: Option<&EditPlan>,
    probe: &VideoProbe,
) -> String {
    build_subtitles_multi(&TranscriptMap::single(transcript.clone()), format, plan, probe)
}

/// Build an SRT or VTT document from a per-source transcript map, optionally applying an edit
/// plan's timeline mapping (empty/absent plan = source timeline). Pure/unit-testable. Handles
/// the multi-source case: each source's words map only into that source's kept segments.
pub(crate) fn build_subtitles_multi(
    transcripts: &TranscriptMap,
    format: &str,
    plan: Option<&EditPlan>,
    probe: &VideoProbe,
) -> String {
    let (segments, speed) = match plan {
        Some(p) => (effective_segments(p, probe), p.speed.unwrap_or(1.0)),
        None => (vec![Segment::keep(0.0, probe.duration_s)], 1.0),
    };
    let lines = group_caption_lines_multi(transcripts, &segments, speed);
    let vtt = format == "vtt";
    let mut out = String::new();
    if vtt {
        out.push_str("WEBVTT\n\n");
    }
    for (i, line) in lines.iter().enumerate() {
        if !vtt {
            out.push_str(&format!("{}\n", i + 1));
        }
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            subtitle_time(line.start, vtt),
            subtitle_time(line.end, vtt),
            line.text()
        ));
    }
    out
}

/// One distinct input source in a (possibly multi-source) render. Index 0 in the slice passed
/// to `build_render_args` is always the MAIN video being edited; any further entries are insert
/// sources (b-roll clips or still-image cards) referenced by `Segment.source`.
pub(crate) struct SourceInput {
    pub path: PathBuf,
    /// Probe of this input. For images `duration_s`/`fps` are 0 and `has_audio` is false, but
    /// `width`/`height` are populated — normalization scales inserts to the MAIN dims regardless.
    pub probe: VideoProbe,
    pub is_image: bool,
}

/// Build the full ffmpeg argument list for a plan. Pure so it can be unit-tested.
/// `inputs[0]` must be the MAIN source; any further entries are insert sources, matched to a
/// `Segment.source` by path. `text_files` must contain one temp-file path per entry in
/// `plan.texts` (drawtext reads the text from a file, which sidesteps filter-graph escaping
/// entirely). `ass_file`, when present, burns those subtitles in after the crop stage.
pub(crate) fn build_render_args(
    inputs: &[SourceInput],
    plan: &EditPlan,
    font: &str,
    text_files: &[PathBuf],
    ass_file: Option<&Path>,
    reframe: Option<&ReframeTrack>,
    encoder: VideoEncoder,
    out: &Path,
) -> Result<Vec<String>, String> {
    let main = inputs.first().ok_or_else(|| "no render inputs".to_string())?;
    let probe = &main.probe;
    let (main_w, main_h) = (probe.width.max(2), probe.height.max(2));
    // fps to normalize insert clips/images to the main timeline's frame rate before concat.
    let target_fps = if probe.fps > 1.0 { probe.fps } else { 30.0 };
    let with_audio = probe.has_audio && !plan.mute;
    let segments: Vec<Segment> = if plan.segments.is_empty() {
        vec![Segment::keep(0.0, probe.duration_s)]
    } else {
        plan.segments.clone()
    };

    // More than the main input means at least one insert → concat has to reconcile audio
    // streams that differ in sample rate/format, so every real-audio branch gets a canonical
    // `aformat` and silent inserts an `anullsrc` at the same params. Single-source renders keep
    // the exact old graph (no aformat), so existing behavior is untouched.
    let multi = inputs.len() > 1;
    const CANON_RATE: u32 = 48_000;
    let aformat = format!("aformat=sample_fmts=fltp:sample_rates={CANON_RATE}:channel_layouts=stereo");

    // Resolve a segment to its input index (None = main = 0), matched by path string.
    let seg_input = |s: &Segment| -> Result<usize, String> {
        match &s.source {
            None => Ok(0),
            Some(src) => inputs
                .iter()
                .position(|i| i.path == PathBuf::from(src.trim()))
                .ok_or_else(|| format!("insert source '{src}' is not among the render inputs")),
        }
    };

    // De-click micro-fades: with 2+ segments, concat stitches independently-decoded audio
    // clips together, and a hard cut mid-waveform is audible as a click/pop. A ~20ms
    // in/out fade on each clip (clamped to half its own duration for very short segments)
    // fixes that with no new EditPlan field — it's always on whenever there's a cut. Silent
    // inserts skip it (nothing to click).
    const MICRO_FADE_S: f64 = 0.02;
    let apply_microfades = with_audio && segments.len() > 1;

    let mut graph = String::new();
    for (i, s) in segments.iter().enumerate() {
        let k = seg_input(s)?;
        let is_img = inputs[k].is_image;
        let seg_dur = (s.end - s.start).max(0.0);
        // ---- video branch ----
        if k == 0 {
            // MAIN source: kept exactly as before (already at the output's base dims/fps/sar).
            graph.push_str(&format!(
                "[0:v]trim=start={:.4}:end={:.4},setpts=PTS-STARTPTS[v{i}];",
                s.start, s.end
            ));
        } else {
            // Insert (b-roll clip or image card): trim, then scale-to-cover + center-crop to the
            // MAIN dims, reset SAR, and normalize fps + pixel format so concat sees uniform
            // frames. Images play from 0.. (start is conventionally 0) off the looped input.
            let (ts, te) = if is_img { (0.0, seg_dur) } else { (s.start, s.end) };
            graph.push_str(&format!(
                "[{k}:v]trim=start={ts:.4}:end={te:.4},setpts=PTS-STARTPTS,\
                 scale={main_w}:{main_h}:force_original_aspect_ratio=increase,\
                 crop={main_w}:{main_h},setsar=1,fps={target_fps:.4},format=yuv420p[v{i}];"
            ));
        }
        // ---- audio branch ----
        if with_audio {
            let real_audio = k == 0 || (!is_img && inputs[k].probe.has_audio);
            if real_audio {
                graph.push_str(&format!(
                    "[{k}:a]atrim=start={:.4}:end={:.4},asetpts=PTS-STARTPTS",
                    s.start, s.end
                ));
                // Denoise per kept segment (simpler and just as effective as denoising the
                // whole track once, without needing an explicit asplit to fan the input out to
                // both a denoise stage and the per-segment atrims).
                if plan.denoise {
                    graph.push_str(",afftdn=nf=-25");
                }
                if multi {
                    graph.push_str(&format!(",{aformat}"));
                }
                if apply_microfades {
                    let fd = MICRO_FADE_S.min(seg_dur / 2.0).max(0.0);
                    if fd > 0.0 {
                        graph.push_str(&format!(
                            ",afade=t=in:st=0:d={fd:.4},afade=t=out:st={:.4}:d={fd:.4}",
                            (seg_dur - fd).max(0.0)
                        ));
                    }
                }
                graph.push_str(&format!("[a{i}];"));
            } else {
                // Silent insert (image card, or a b-roll clip with no audio track): synthesize
                // matching silence so concat's audio pad count stays balanced.
                graph.push_str(&format!(
                    "anullsrc=channel_layout=stereo:sample_rate={CANON_RATE},\
                     atrim=end={seg_dur:.4},asetpts=PTS-STARTPTS,{aformat}[a{i}];"
                ));
            }
        }
    }
    for i in 0..segments.len() {
        graph.push_str(&format!("[v{i}]"));
        if with_audio {
            graph.push_str(&format!("[a{i}]"));
        }
    }
    graph.push_str(&format!(
        "concat=n={}:v=1:a={}[cv]{}",
        segments.len(),
        if with_audio { "1" } else { "0" },
        if with_audio { "[ca]" } else { "" }
    ));

    // Video post-chain: preset → speed → aspect crop → text overlays. Speed is applied
    // before drawtext so overlay times land on the final output timeline.
    let mut post = Vec::new();
    if let Some(key) = plan.filter.as_deref() {
        let chain = preset_chain(key)?;
        if !chain.is_empty() {
            post.push(chain.to_string());
        }
    }
    if let Some(speed) = plan.speed.filter(|s| (*s - 1.0).abs() > 1e-6) {
        post.push(format!("setpts=PTS/{speed:.6}"));
    }
    if let Some(a) = plan.aspect.as_deref() {
        let (aw, ah) = parse_aspect(a)?;
        // Subject-aware crop replaces the static center-crop when a reframe track was computed
        // (smart_reframe + a narrowing aspect + detected faces). The x expression is on the
        // OUTPUT timeline, matching where this crop sits in the graph (after concat+setpts).
        // Single-quote the x/y values so the nested if()'s commas aren't parsed as filter
        // separators. Everything falls back to the static center-crop otherwise.
        match reframe {
            Some(rf) => post.push(format!(
                "crop={cw}:{ch}:x='{expr}':y='{y}'",
                cw = rf.crop_w,
                ch = rf.crop_h,
                expr = rf.x_expr,
                y = rf.crop_y
            )),
            None => post.push(format!("crop=min(iw\\,ih*{aw}/{ah}):min(ih\\,iw*{ah}/{aw})")),
        }
        post.push("scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string());
    }
    // Burn-in captions AFTER speed (setpts) and crop so the ASS output-timeline timestamps
    // and PlayResX/Y line up with the frames. Escape filtergraph specials in the path
    // defensively (the temp path is already safe).
    if let Some(ass) = ass_file {
        let escaped = ass
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace(':', "\\:")
            .replace('\'', "\\'");
        post.push(format!("subtitles=filename={escaped}"));
    }
    for (t, file) in plan.texts.iter().zip(text_files) {
        let size = t.size.unwrap_or(48).clamp(8, 200);
        post.push(format!(
            "drawtext=fontfile={font}:textfile={}:fontsize={size}:fontcolor=white:borderw=3:bordercolor=black@0.6:{}:enable=between(t\\,{:.3}\\,{:.3})",
            file.display(),
            drawtext_position(t.position.as_deref()),
            t.start,
            t.end
        ));
    }
    // Overall fade-in/out is the very last video stage, over the fully composed frame
    // (post-crop, post-captions, post-text).
    if let Some(fade) = plan.fade {
        let out_dur = plan.output_duration(probe);
        let fade_out_start = (out_dur - fade).max(0.0);
        post.push(format!("fade=t=in:st=0:d={fade:.4}"));
        post.push(format!("fade=t=out:st={fade_out_start:.4}:d={fade:.4}"));
    }
    if post.is_empty() {
        graph.push_str(";[cv]null[vout]");
    } else {
        graph.push_str(&format!(";[cv]{}[vout]", post.join(",")));
    }
    if with_audio {
        // Audio post-chain, in order: atempo (speed change) → overall fade → loudnorm.
        // Denoise and the per-segment de-click micro-fades already ran before concat, so
        // loudnorm's loudness measurement sees the final, speed-corrected, faded mix.
        let mut apost = Vec::new();
        if let Some(speed) = plan.speed.filter(|s| (*s - 1.0).abs() > 1e-6) {
            apost.push(atempo_chain(speed));
        }
        if let Some(fade) = plan.fade {
            let out_dur = plan.output_duration(probe);
            let fade_out_start = (out_dur - fade).max(0.0);
            apost.push(format!("afade=t=in:st=0:d={fade:.4}"));
            apost.push(format!("afade=t=out:st={fade_out_start:.4}:d={fade:.4}"));
        }
        if plan.normalize_audio {
            apost.push("loudnorm=I=-14:TP=-1.5:LRA=11".to_string());
        }
        if apost.is_empty() {
            graph.push_str(";[ca]anull[aout]");
        } else {
            graph.push_str(&format!(";[ca]{}[aout]", apost.join(",")));
        }
    }

    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostdin".into(),
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
    ];
    // One `-i` per distinct input, in index order (so `[k:v]`/`[k:a]` line up). Image inputs
    // are looped into a finite clip long enough for the longest segment that uses them.
    for (k, inp) in inputs.iter().enumerate() {
        if inp.is_image {
            let max_dur = segments
                .iter()
                .filter(|s| seg_input(s).ok() == Some(k))
                .map(|s| (s.end - s.start).max(0.0))
                .fold(0.0_f64, f64::max)
                .max(0.2);
            args.extend([
                "-loop".into(),
                "1".into(),
                "-framerate".into(),
                format!("{target_fps:.4}"),
                "-t".into(),
                format!("{max_dur:.4}"),
                "-i".into(),
                inp.path.to_string_lossy().to_string(),
            ]);
        } else {
            args.extend(["-i".into(), inp.path.to_string_lossy().to_string()]);
        }
    }
    args.extend([
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "[vout]".into(),
    ]);
    if with_audio {
        args.extend(["-map".into(), "[aout]".into(), "-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]);
    } else {
        args.push("-an".into());
    }
    match encoder {
        VideoEncoder::Libx264 => {
            args.extend([
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                "veryfast".into(),
                "-crf".into(),
                "20".into(),
            ]);
        }
        VideoEncoder::H264Videotoolbox => {
            let (out_w, out_h) = output_resolution(plan, probe);
            let bps = videotoolbox_bitrate_bps(out_w, out_h, probe.fps);
            args.extend(["-c:v".into(), "h264_videotoolbox".into(), "-b:v".into(), bps.to_string()]);
        }
    }
    args.extend([
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.to_string_lossy().to_string(),
    ]);
    Ok(args)
}

// ---------- smart reframe (subject-aware crop) ----------
//
// When `EditPlan.aspect` makes the output narrower than the source, a static center-crop
// decapitates off-center speakers. Smart reframe detects faces on-device (the
// `muninn-framescan` Swift sidecar), computes a piecewise-constant HORIZONTAL crop track
// that keeps the subject in frame, and emits it as a compact ffmpeg crop-x expression. Y
// stays centered (vertical crops for wide targets are rare and unsupported here). Any
// failure — missing sidecar, decode error, or simply no faces — falls back silently to the
// static center-crop, so a render is never blocked by reframing.

/// A detected face in one sampled frame. Normalized [0,1], TOP-LEFT origin, `c` = confidence.
#[derive(Deserialize, Clone, Debug)]
pub(crate) struct Face {
    pub x: f64,
    /// Parsed from the sidecar but unused: only horizontal (x) reframing is supported today
    /// (vertical crops for wide targets are rare). Kept so the JSON schema stays complete.
    #[allow(dead_code)]
    pub y: f64,
    pub w: f64,
    pub h: f64,
    #[serde(default)]
    pub c: f64,
}

/// Faces detected in one sampled frame at source time `t` (seconds).
#[derive(Deserialize, Clone, Debug)]
pub(crate) struct FrameFaces {
    pub t: f64,
    #[serde(default)]
    pub faces: Vec<Face>,
}

#[derive(Deserialize, Debug, Default)]
struct FrameScanResult {
    #[serde(default)]
    frames: Vec<FrameFaces>,
}

/// The computed dynamic crop for a render: fixed crop box `crop_w`×`crop_h` at a fixed
/// vertical offset `crop_y`, with a horizontal `x_expr` (an ffmpeg expression over OUTPUT
/// time `t`) that re-frames onto the subject. Consumed by `build_render_args`.
#[derive(Clone, Debug)]
pub(crate) struct ReframeTrack {
    pub crop_w: u32,
    pub crop_h: u32,
    pub crop_y: u32,
    pub x_expr: String,
}

// --- tuning constants ---
/// Subject must leave the middle 40% of the current window to trigger a move — this is the
/// half-band as a fraction of the window width (0.2 → 40% total).
const REFRAME_EXIT_FRAC: f64 = 0.2;
/// Subject must stay outside the band this long (source seconds) before the crop jumps.
const REFRAME_DWELL_S: f64 = 0.5;
/// Output-timeline windows shorter than this are merged into a neighbor.
const REFRAME_MIN_WINDOW_S: f64 = 1.0;
/// Hard cap on distinct crop windows (keeps the ffmpeg expression bounded).
const REFRAME_MAX_WINDOWS: usize = 60;
/// Frames-per-second sampled by the face-detection sidecar.
const REFRAME_SCAN_FPS: f64 = 2.0;

fn floor_even(v: f64) -> u32 {
    let n = v.max(2.0) as u32;
    n - (n % 2)
}

/// Crop box dimensions (even pixels) for `aspect` applied to the source — mirrors
/// `output_resolution`'s crop math.
fn crop_dims(probe: &VideoProbe, aw: u32, ah: u32) -> (u32, u32) {
    let (iw, ih) = (probe.width as f64, probe.height as f64);
    let cw = iw.min(ih * aw as f64 / ah as f64);
    let ch = ih.min(iw * ah as f64 / aw as f64);
    (floor_even(cw), floor_even(ch))
}

/// Locate the compiled `muninn-framescan` sidecar (see speech/framescan.swift). Same
/// lookup as `transcriber_helper_path`: next to the app binary in bundled builds, else the
/// build.rs output in src-tauri/binaries/ for dev and `cargo test`.
fn framescan_helper_path() -> Option<PathBuf> {
    let names = [
        "muninn-framescan".to_string(),
        format!("muninn-framescan-{}", env!("MUNINN_TARGET_TRIPLE")),
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

/// Run the face-detection sidecar over `src`, sampling at `fps`. Blocking. Errors (missing
/// binary, decode failure, bad JSON) propagate so the caller can fall back to center crop.
fn scan_faces(src: &Path, fps: f64) -> Result<Vec<FrameFaces>, String> {
    let helper = framescan_helper_path()
        .ok_or_else(|| "framescan helper not found (compiled by build.rs — needs Xcode)".to_string())?;
    let out = Command::new(&helper)
        .arg(src)
        .arg("--fps")
        .arg(format!("{fps}"))
        .output()
        .map_err(|e| format!("failed to start {}: {e}", helper.display()))?;
    if !out.status.success() {
        return Err(format!("framescan failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let r: FrameScanResult =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("framescan returned invalid JSON: {e}"))?;
    Ok(r.frames)
}

/// Confidence-and-area-weighted horizontal subject center per frame (normalized). The
/// largest face dominates (weight = confidence × area). `None` when a frame has no faces.
fn subject_centers(frames: &[FrameFaces]) -> Vec<(f64, Option<f64>)> {
    frames
        .iter()
        .map(|fr| {
            let mut wsum = 0.0;
            let mut xsum = 0.0;
            for f in &fr.faces {
                let weight = f.c.max(0.0) * (f.w * f.h).max(0.0);
                if weight > 0.0 {
                    wsum += weight;
                    xsum += weight * (f.x + f.w / 2.0);
                }
            }
            (fr.t, if wsum > 0.0 { Some(xsum / wsum) } else { None })
        })
        .collect()
}

/// A source-timeline window holding a constant subject center `cx` (normalized).
#[derive(Clone, Debug, PartialEq)]
struct SrcWin {
    start: f64,
    end: f64,
    cx: f64,
}

/// Hysteresis: walk the per-frame subject centers and only commit a new crop position when
/// the subject leaves the middle band of the current window (`win_norm` wide) for at least
/// `REFRAME_DWELL_S`. Frames with no face hold the previous center. Returns contiguous
/// source-time windows covering `[0, duration]`, or empty when no face was ever detected.
fn hysteresis_windows(centers: &[(f64, Option<f64>)], win_norm: f64, duration: f64) -> Vec<SrcWin> {
    let Some(first) = centers.iter().find_map(|(_, c)| *c) else {
        return Vec::new();
    };
    let band = REFRAME_EXIT_FRAC * win_norm;
    let mut wins: Vec<SrcWin> = Vec::new();
    let mut cur = first;
    let mut start = 0.0_f64;
    let mut last_cx = first;
    let mut outside_since: Option<f64> = None;

    for (t, c) in centers {
        let cx = c.unwrap_or(last_cx);
        last_cx = cx;
        if (cx - cur).abs() > band {
            let os = *outside_since.get_or_insert(*t);
            if *t - os >= REFRAME_DWELL_S {
                // Commit: the crop held `cur` until the subject left (os); jump to the subject.
                wins.push(SrcWin { start, end: os, cx: cur });
                start = os;
                cur = cx;
                outside_since = None;
            }
        } else {
            outside_since = None;
        }
    }
    wins.push(SrcWin { start, end: duration.max(start), cx: cur });
    wins
}

/// One piece of the OUTPUT-timeline crop track: from `out_start` (output seconds) the crop
/// x is `x` pixels, until the next piece's `out_start`.
#[derive(Clone, Debug, PartialEq)]
struct OutPiece {
    out_start: f64,
    x: f64,
}

/// Convert a normalized subject center to a clamped pixel crop-x that centers the crop box
/// (`cw` wide) on the subject within a `in_w`-wide frame.
fn crop_x_for(cx_norm: f64, in_w: f64, cw: u32, max_x: f64) -> f64 {
    ((cx_norm * in_w) - cw as f64 / 2.0).round().clamp(0.0, max_x)
}

/// Map source-time subject windows onto the OUTPUT timeline, walking the effective segments
/// in playback order. Insert segments (source set) carry no detections, so their whole
/// output slot is centered. This is where the "detection times are SOURCE, crop is OUTPUT"
/// mapping happens — the same offset/speed logic as `map_interval_to_output`.
fn build_output_pieces(
    src_wins: &[SrcWin],
    segments: &[Segment],
    speed: f64,
    in_w: f64,
    cw: u32,
    max_x: f64,
    center_x: f64,
) -> Vec<OutPiece> {
    let speed = if speed > 0.0 { speed } else { 1.0 };
    let mut pieces: Vec<OutPiece> = Vec::new();
    let mut out_off = 0.0_f64;
    for s in segments {
        let dur = (s.end - s.start).max(0.0);
        if s.source.is_some() {
            // Insert (b-roll/image): no face data — hold centered for the whole slot.
            pieces.push(OutPiece { out_start: out_off, x: center_x });
        } else {
            let mut emitted = false;
            for w in src_wins {
                let os = w.start.max(s.start);
                let oe = w.end.min(s.end);
                if oe - os > 1e-6 {
                    let out_t = out_off + (os - s.start) / speed;
                    pieces.push(OutPiece { out_start: out_t, x: crop_x_for(w.cx, in_w, cw, max_x) });
                    emitted = true;
                }
            }
            if !emitted {
                pieces.push(OutPiece { out_start: out_off, x: center_x });
            }
        }
        out_off += dur / speed;
    }
    pieces.sort_by(|a, b| a.out_start.partial_cmp(&b.out_start).unwrap_or(std::cmp::Ordering::Equal));
    // Drop consecutive pieces that don't change the crop.
    let mut merged: Vec<OutPiece> = Vec::with_capacity(pieces.len());
    for p in pieces {
        if merged.last().map_or(false, |l: &OutPiece| (l.x - p.x).abs() < 0.5) {
            continue;
        }
        merged.push(p);
    }
    if let Some(f) = merged.first_mut() {
        f.out_start = 0.0;
    }
    merged
}

/// Merge output pieces shorter than `min_len`, and cap the total at `max_n` by repeatedly
/// absorbing the shortest window into its closest-x (smallest visual jump) neighbor. Clamps
/// every x into `[0, max_x]`.
fn post_process_pieces(mut pieces: Vec<OutPiece>, out_dur: f64, min_len: f64, max_n: usize, max_x: f64) -> Vec<OutPiece> {
    for p in &mut pieces {
        p.x = p.x.clamp(0.0, max_x);
    }
    if pieces.is_empty() {
        return vec![OutPiece { out_start: 0.0, x: 0.0 }];
    }
    loop {
        if pieces.len() <= 1 {
            break;
        }
        // Find the shortest window (duration to the next boundary, or out_dur for the last).
        let mut worst: Option<(usize, f64)> = None;
        for i in 0..pieces.len() {
            let end = if i + 1 < pieces.len() { pieces[i + 1].out_start } else { out_dur };
            let d = end - pieces[i].out_start;
            if worst.map_or(true, |(_, wd)| d < wd) {
                worst = Some((i, d));
            }
        }
        let (i, d) = worst.unwrap();
        if d >= min_len && pieces.len() <= max_n {
            break;
        }
        // Absorb piece i into whichever neighbor is the smaller crop jump.
        let into_prev = if i == 0 {
            false
        } else if i + 1 >= pieces.len() {
            true
        } else {
            (pieces[i - 1].x - pieces[i].x).abs() <= (pieces[i + 1].x - pieces[i].x).abs()
        };
        if into_prev {
            pieces.remove(i); // previous window extends over the removed one
        } else {
            let start_i = pieces[i].out_start;
            pieces[i + 1].out_start = start_i; // next window extends back over the removed one
            pieces.remove(i);
        }
    }
    if let Some(f) = pieces.first_mut() {
        f.out_start = 0.0;
    }
    pieces
}

/// Emit the crop-x pieces as a nested ffmpeg `if(lt(t,T),X,...)` expression over OUTPUT time.
fn pieces_to_expr(pieces: &[OutPiece]) -> String {
    if pieces.is_empty() {
        return "0".to_string();
    }
    let mut expr = format!("{}", pieces.last().unwrap().x.round() as i64);
    for i in (1..pieces.len()).rev() {
        let t = pieces[i].out_start;
        let x = pieces[i - 1].x.round() as i64;
        expr = format!("if(lt(t\\,{t:.3})\\,{x}\\,{expr})");
    }
    expr
}

/// Compute a reframe crop track from detected faces. Pure/unit-testable. Returns `None` (→
/// static center crop) when the aspect doesn't narrow horizontally or no face was detected.
pub(crate) fn compute_reframe_track(frames: &[FrameFaces], probe: &VideoProbe, plan: &EditPlan) -> Option<ReframeTrack> {
    let (aw, ah) = plan.aspect.as_deref().and_then(|a| parse_aspect(a).ok())?;
    let (cw, ch) = crop_dims(probe, aw, ah);
    if cw >= probe.width {
        return None; // not narrowing horizontally — nothing to reframe
    }
    let in_w = probe.width as f64;
    let win_norm = cw as f64 / in_w;
    let max_x = (probe.width - cw) as f64;
    let center_x = (max_x / 2.0).round();
    let crop_y = floor_even((probe.height.saturating_sub(ch)) as f64 / 2.0).min(probe.height.saturating_sub(ch));

    let centers = subject_centers(frames);
    let src_wins = hysteresis_windows(&centers, win_norm, probe.duration_s.max(0.0));
    if src_wins.is_empty() {
        return None; // no faces detected anywhere
    }

    let segments = effective_segments(plan, probe);
    let speed = plan.speed.unwrap_or(1.0);
    let pieces = build_output_pieces(&src_wins, &segments, speed, in_w, cw, max_x, center_x);
    let out_dur = plan.output_duration(probe).max(0.01);
    let pieces = post_process_pieces(pieces, out_dur, REFRAME_MIN_WINDOW_S, REFRAME_MAX_WINDOWS, max_x);

    Some(ReframeTrack { crop_w: cw, crop_h: ch, crop_y, x_expr: pieces_to_expr(&pieces) })
}

/// Run the face scan and compute a reframe track for a render, or `None` to keep the static
/// center-crop. Only fires when `smart_reframe` is set AND the aspect narrows the frame; any
/// scan failure logs to stderr and falls back to center crop rather than failing the render.
pub(crate) fn compute_reframe_for_render(src: &Path, probe: &VideoProbe, plan: &EditPlan) -> Option<ReframeTrack> {
    if !plan.smart_reframe {
        return None;
    }
    let (aw, ah) = plan.aspect.as_deref().and_then(|a| parse_aspect(a).ok())?;
    let (cw, _ch) = crop_dims(probe, aw, ah);
    if cw >= probe.width {
        return None; // aspect doesn't narrow → smart_reframe is a harmless no-op
    }
    let frames = match scan_faces(src, REFRAME_SCAN_FPS) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[reframe] face scan failed, falling back to center crop: {e}");
            return None;
        }
    };
    compute_reframe_track(&frames, probe, plan)
}

// ---------- render (blocking; call from spawn_blocking) ----------

#[derive(Serialize, Clone)]
pub struct RenderProgress {
    pub job_id: String,
    pub pct: f64,
    pub seconds: f64,
}

/// job_id → ffmpeg pid, for cancellation.
fn renders() -> &'static Mutex<HashMap<String, u32>> {
    static RENDERS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    RENDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cancel_render(job_id: &str) -> bool {
    let pid = renders().lock().ok().and_then(|m| m.get(job_id).copied());
    match pid {
        Some(pid) => {
            let _ = Command::new("/bin/kill").args(["-9", &pid.to_string()]).status();
            true
        }
        None => false,
    }
}

/// Kill every registered render whose `job_id` starts with `prefix` (SIGKILL). Used to cancel
/// the active per-short sub-render of a shorts job, whose ids are `{job}-1..n`. Returns whether
/// any process was killed.
fn cancel_render_prefix(prefix: &str) -> bool {
    let pids: Vec<u32> = renders()
        .lock()
        .ok()
        .map(|m| m.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(_, v)| *v).collect())
        .unwrap_or_default();
    let killed = !pids.is_empty();
    for pid in pids {
        let _ = Command::new("/bin/kill").args(["-9", &pid.to_string()]).status();
    }
    killed
}

/// Parent-job-id set flagged for cancellation by the multi-step pipelines (auto-shorts and
/// compile). Each pipeline checks `is_pipeline_cancelled` between steps (mirrors
/// `AppState.stop_requests` for chat) and clears its own id on exit.
fn pipeline_cancels() -> &'static Mutex<std::collections::HashSet<String>> {
    static CANCELS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    CANCELS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn flag_pipeline_cancel(job_id: &str) {
    if let Ok(mut s) = pipeline_cancels().lock() {
        s.insert(job_id.to_string());
    }
}

fn is_pipeline_cancelled(job_id: &str) -> bool {
    pipeline_cancels().lock().map(|s| s.contains(job_id)).unwrap_or(false)
}

fn clear_pipeline_cancel(job_id: &str) {
    if let Ok(mut s) = pipeline_cancels().lock() {
        s.remove(job_id);
    }
}

/// Flag a shorts job for cancellation AND kill its currently-rendering sub-render, so a cancel
/// mid-render stops immediately rather than only at the next between-shorts boundary. Sub-render
/// ids are `{job}-1..n`. Returns whether an active sub-render was killed.
pub(crate) fn request_cancel_shorts(job_id: &str) -> bool {
    flag_pipeline_cancel(job_id);
    cancel_render_prefix(&format!("{job_id}-"))
}

/// Flag a compile job for cancellation AND kill its (single) active render, which is keyed by
/// the compile job_id directly. Returns whether the render was killed.
pub(crate) fn request_cancel_compile(job_id: &str) -> bool {
    flag_pipeline_cancel(job_id);
    cancel_render(job_id)
}

pub(crate) fn sanitize_stem(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "edit".to_string() } else { s }
}

/// Result of one ffmpeg render attempt (used by `render_edit`'s hardware→software retry).
struct FfmpegAttempt {
    status: std::process::ExitStatus,
    stderr: String,
    elapsed: std::time::Duration,
}

/// Take the last `n` lines of ffmpeg stderr, in order (for compact error messages).
fn stderr_tail(stderr: &str, n: usize) -> String {
    stderr.lines().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Spawn ffmpeg with `args`, register its pid under `job_id` (so `cancel_render` can kill
/// it), stream the `-progress` stdout as `video-render-progress` events, and wait for exit.
fn run_ffmpeg_attempt(app: &AppHandle, job_id: &str, args: &[String], total_s: f64) -> Result<FfmpegAttempt, String> {
    let start = std::time::Instant::now();
    let mut child = Command::new(ffmpeg_path()?)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg failed to start: {e}"))?;

    if let Ok(mut m) = renders().lock() {
        m.insert(job_id.to_string(), child.id());
    }

    // Drain stderr on a side thread (avoids pipe-full deadlock) while we parse the
    // -progress key=value stream from stdout on this one.
    let mut stderr = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_string(&mut buf);
        }
        buf
    });

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            // out_time_ms is microseconds despite the name.
            if let Some(us) = line.strip_prefix("out_time_ms=").and_then(|v| v.trim().parse::<f64>().ok()) {
                let seconds = (us / 1_000_000.0).max(0.0);
                let pct = (seconds / total_s * 100.0).clamp(0.0, 100.0);
                let _ = app.emit("video-render-progress", RenderProgress {
                    job_id: job_id.to_string(),
                    pct,
                    seconds,
                });
            }
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    if let Ok(mut m) = renders().lock() {
        m.remove(job_id);
    }
    let stderr_text = stderr_thread.join().unwrap_or_default();
    Ok(FfmpegAttempt { status, stderr: stderr_text, elapsed: start.elapsed() })
}

/// Render an edit plan to `out_dir`, emitting `video-render-progress` events keyed by
/// `job_id`. Blocking — run inside `tauri::async_runtime::spawn_blocking`.
///
/// `prefer_hw_encode` selects Apple's `h264_videotoolbox` hardware encoder when it's
/// available (see `pick_encoder`). If a hardware attempt fails almost immediately — a
/// signature of the encoder session failing to initialize rather than a real encode
/// error — the whole render is retried once with the software `libx264` encoder before
/// the failure is reported. A render killed via `cancel_render` (SIGKILL) is never retried.
pub(crate) fn render_edit(
    app: &AppHandle,
    job_id: &str,
    src: &Path,
    plan: &EditPlan,
    transcripts: &TranscriptMap,
    out_dir: &Path,
    prefer_hw_encode: bool,
) -> Result<PathBuf, String> {
    let probe = probe_video(src)?;
    let mut plan = plan.clone();
    plan.validate(&probe)?;

    // Probe each distinct source once. Index 0 is always the MAIN video; insert sources
    // (b-roll clips and image cards referenced by `Segment.source`) follow, deduped by path.
    // Existence/probe errors surface here with the offending path — `validate()` can't touch
    // the filesystem, so per-file checks live here.
    let mut inputs: Vec<SourceInput> = vec![SourceInput {
        path: src.to_path_buf(),
        probe: probe.clone(),
        is_image: false,
    }];
    let mut seen_sources: HashMap<String, usize> = HashMap::new();
    for seg in &plan.segments {
        if let Some(raw) = &seg.source {
            let key = raw.trim().to_string();
            if key.is_empty() || seen_sources.contains_key(&key) {
                continue;
            }
            let path = PathBuf::from(&key);
            let is_image = is_image_source(&key);
            let sp = probe_video(&path).map_err(|e| format!("insert source '{key}': {e}"))?;
            seen_sources.insert(key, inputs.len());
            inputs.push(SourceInput { path, probe: sp, is_image });
        }
    }

    let font = font_path().ok_or_else(|| "no usable system font found for text overlays".to_string())?;
    let total_s = plan.output_duration(&probe).max(0.01);

    // drawtext reads each overlay's text from a temp file — no escaping games. The burned
    // caption ASS lives in the same temp dir and stays alive until the render finishes.
    let text_dir = std::env::temp_dir().join(format!("muninn-video-{job_id}"));
    std::fs::create_dir_all(&text_dir).map_err(|e| e.to_string())?;
    let mut text_files = Vec::new();
    for (i, t) in plan.texts.iter().enumerate() {
        let p = text_dir.join(format!("text-{i}.txt"));
        std::fs::write(&p, t.text.as_bytes()).map_err(|e| e.to_string())?;
        text_files.push(p);
    }

    let ass_file = if plan.captions {
        if transcripts.has_captions() {
            let p = text_dir.join("captions.ass");
            std::fs::write(&p, build_ass_multi(transcripts, &plan, &probe).as_bytes()).map_err(|e| e.to_string())?;
            Some(p)
        } else {
            let _ = std::fs::remove_dir_all(&text_dir);
            return Err(
                "captions were requested but no transcript is available — transcribe the video first"
                    .to_string(),
            );
        }
    } else {
        None
    };

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stem = sanitize_stem(plan.output_name.as_deref().unwrap_or_else(|| {
        src.file_stem().and_then(|s| s.to_str()).unwrap_or("edit")
    }));
    let out_path = out_dir.join(format!("{stem}-{stamp}.mp4"));

    // Smart reframe (subject-aware crop): scan the MAIN source for faces and compute a crop
    // track. Returns None (→ static center crop) unless smart_reframe is on and the aspect
    // narrows the frame; any scan failure falls back silently. Insert sources are never
    // face-tracked (their output slots stay centered — see build_output_pieces).
    let reframe = compute_reframe_for_render(src, &probe, &plan);

    let encoder = pick_encoder(prefer_hw_encode);
    let mut used_encoder = encoder;
    let args =
        build_render_args(&inputs, &plan, font, &text_files, ass_file.as_deref(), reframe.as_ref(), encoder, &out_path)?;
    let mut attempt = run_ffmpeg_attempt(app, job_id, &args, total_s)?;
    let mut fallback_note = String::new();

    if !attempt.status.success()
        && encoder == VideoEncoder::H264Videotoolbox
        && attempt.elapsed.as_secs_f64() < 2.0
        && !was_signal_killed(&attempt.status)
    {
        // A hardware VideoToolbox session can fail to initialize (codec busy, unsupported
        // profile, no available hardware encode slots, ...) — that shows up as a near-instant
        // nonzero exit. Retry the whole render once in software rather than surfacing a
        // confusing hardware-only failure.
        let hw_tail = stderr_tail(&attempt.stderr, 6);
        let _ = std::fs::remove_file(&out_path);
        let fallback_args = build_render_args(
            &inputs,
            &plan,
            font,
            &text_files,
            ass_file.as_deref(),
            reframe.as_ref(),
            VideoEncoder::Libx264,
            &out_path,
        )?;
        attempt = run_ffmpeg_attempt(app, job_id, &fallback_args, total_s)?;
        used_encoder = VideoEncoder::Libx264;
        fallback_note = format!(
            "hardware encoder (h264_videotoolbox) failed, retried with software (libx264):\n{hw_tail}\n---\n"
        );
    }

    let _ = std::fs::remove_dir_all(&text_dir);

    if !attempt.status.success() {
        let _ = std::fs::remove_file(&out_path);
        let tail = stderr_tail(&attempt.stderr, 12);
        let encoder_name = if used_encoder == VideoEncoder::H264Videotoolbox { "h264_videotoolbox" } else { "libx264" };
        return Err(format!("ffmpeg render failed [encoder: {encoder_name}] (was it cancelled?):\n{fallback_note}{tail}"));
    }
    let _ = app.emit("video-render-progress", RenderProgress {
        job_id: job_id.to_string(),
        pct: 100.0,
        seconds: total_s,
    });
    Ok(out_path)
}

// ---------- auto-shorts (highlight → vertical clip pipeline) ----------

/// Default target duration (seconds) for one generated short when the caller gives no
/// `duration_hint_s`. Sits in the sweet spot for a Reels/Shorts/TikTok clip.
pub(crate) const DEFAULT_SHORT_TARGET_S: f64 = 45.0;

/// One LLM-picked highlight: a self-contained moment to cut into a vertical short. `segments`
/// are SOURCE-timeline keep-ranges (MAIN video only — never inserts). `hook` is the opening
/// line the clip banks on; `rationale` is the model's reasoning (surfaced for transparency).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ShortSpec {
    pub title: String,
    pub segments: Vec<Segment>,
    pub hook: String,
    pub rationale: String,
}

/// One rendered short returned to the caller.
#[derive(Serialize, Clone, Debug)]
pub struct ShortResult {
    pub title: String,
    pub hook: String,
    pub path: String,
    pub duration_s: f64,
}

/// Pipeline stage event so the UI can show which phase a shorts job is in.
#[derive(Serialize, Clone)]
struct ShortsProgress {
    job_id: String,
    current: usize,
    total: usize,
    title: String,
    /// "transcribing" | "planning" | "rendering" | "done".
    stage: String,
}

/// Validate + normalize LLM-produced shorts specs against the probed source, returning the
/// best `count` usable specs. Pure so it's unit-testable without an LLM. Rules:
/// - the model must return 1..=`count`*2 specs (fewer than 1 or a wild overflow ⇒ error);
/// - each spec's MAIN-source segments are clamped into `[0, duration]`, sorted, and must be
///   non-empty and non-overlapping (a spec that overlaps itself or clamps to nothing is dropped);
/// - each spec's total kept duration must land within 0.5×–2× of `target_s` (else dropped);
/// - titles are trimmed to ≤60 chars; any `source` (insert) on a segment is stripped — shorts
///   are cut from the main video only.
/// After filtering, the first `count` survivors are returned (the model is told to order best-first).
pub(crate) fn sanitize_short_specs(
    specs: Vec<ShortSpec>,
    probe: &VideoProbe,
    count: usize,
    target_s: f64,
) -> Result<Vec<ShortSpec>, String> {
    let count = count.max(1);
    if specs.is_empty() {
        return Err("the model returned no shorts".to_string());
    }
    if specs.len() > count * 2 {
        return Err(format!(
            "the model returned {} shorts (expected at most {})",
            specs.len(),
            count * 2
        ));
    }
    let dur = probe.duration_s.max(0.0);
    let min_total = 0.5 * target_s;
    let max_total = 2.0 * target_s;

    let mut valid: Vec<ShortSpec> = Vec::new();
    for mut spec in specs {
        // Clamp segments into the source, main-source only, dropping empties.
        let mut segs: Vec<Segment> = spec
            .segments
            .iter()
            .map(|s| {
                let start = s.start.max(0.0);
                let end = s.end.min(dur);
                Segment::keep(start, end)
            })
            .filter(|s| s.end - s.start >= 0.5)
            .collect();
        if segs.is_empty() {
            continue;
        }
        segs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
        // Reject a spec whose segments overlap each other (playback would double-cover time).
        let overlaps = segs.windows(2).any(|w| w[1].start < w[0].end - 1e-6);
        if overlaps {
            continue;
        }
        let total: f64 = segs.iter().map(|s| s.end - s.start).sum();
        if total < min_total || total > max_total {
            continue;
        }
        spec.segments = segs;
        spec.title = {
            let t: String = spec.title.trim().chars().take(60).collect();
            if t.is_empty() { "Untitled clip".to_string() } else { t }
        };
        valid.push(spec);
        if valid.len() >= count {
            break;
        }
    }
    if valid.is_empty() {
        return Err(format!(
            "no usable shorts after validation (need clips of ~{:.0}s within {:.0}–{:.0}s, non-overlapping, inside the video)",
            target_s, min_total, max_total
        ));
    }
    Ok(valid)
}

/// The full auto-shorts pipeline: transcribe → LLM highlight picking → sequential vertical
/// renders into the Content library. Shared by the Studio command (`video_generate_shorts`)
/// and the `generate_shorts` agent tool so both behave identically.
///
/// Emits `shorts-progress` events (`{job_id, current, total, title, stage}`) at each phase, and
/// per-short `video-render-progress` events keyed `{job_id}-{n}` (so the existing render bar
/// works). Renders run ONE AT A TIME to avoid saturating ffmpeg/VideoToolbox. Cancellation is
/// cooperative: `request_cancel_shorts(job_id)` both flags the job (checked between shorts) and
/// SIGKILLs the active sub-render. A cancel mid-run returns whatever shorts already finished.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_shorts_pipeline(
    app: &AppHandle,
    state: &AppState,
    src: &Path,
    count: u32,
    duration_hint_s: Option<f64>,
    brief: Option<&str>,
    job_id: &str,
    captions: bool,
    skip_polish: bool,
) -> Result<Vec<ShortResult>, String> {
    clear_pipeline_cancel(job_id); // clear any stale flag from a previous job reusing this id
    let count = count.clamp(1, 8) as usize;

    let src_owned = src.to_path_buf();
    let probe = tauri::async_runtime::spawn_blocking(move || probe_video(&src_owned))
        .await
        .map_err(|e| e.to_string())??;
    if !probe.has_audio {
        return Err("this video has no audio track — auto-shorts needs speech to find highlights".to_string());
    }

    let emit_stage = |current: usize, total: usize, title: &str, stage: &str| {
        let _ = app.emit(
            "shorts-progress",
            ShortsProgress {
                job_id: job_id.to_string(),
                current,
                total,
                title: title.to_string(),
                stage: stage.to_string(),
            },
        );
    };

    emit_stage(0, count, "", "transcribing");
    let transcript = transcribe_video(state, src).await?;
    if transcript.segments.is_empty() {
        return Err("transcription produced no speech segments to build shorts from".to_string());
    }
    if is_pipeline_cancelled(job_id) {
        clear_pipeline_cancel(job_id);
        emit_stage(0, count, "", "done");
        return Ok(Vec::new());
    }

    emit_stage(0, count, "", "planning");
    let specs =
        crate::llm::generate_shorts_specs(state, &transcript, &probe, count, duration_hint_s, brief).await?;

    let prefer_hw = hw_encode_preferred(state);
    let out_dir = crate::tools::content_dir(app)?;
    let total = specs.len();
    let mut results: Vec<ShortResult> = Vec::new();
    let mut last_err: Option<String> = None;

    for (i, spec) in specs.iter().enumerate() {
        if is_pipeline_cancelled(job_id) {
            break;
        }
        emit_stage(i + 1, total, &spec.title, "rendering");
        let sub_job = format!("{job_id}-{}", i + 1);
        let plan = EditPlan {
            segments: spec.segments.clone(),
            aspect: Some("9:16".to_string()),
            smart_reframe: true,
            captions,
            caption_style: if captions { Some("karaoke".to_string()) } else { None },
            normalize_audio: !skip_polish,
            fade: if skip_polish { None } else { Some(0.3) },
            output_name: Some(sanitize_stem(&spec.title)),
            ..Default::default()
        };
        let dur: f64 = spec.segments.iter().map(|s| (s.end - s.start).max(0.0)).sum();

        let app_c = app.clone();
        let transcript_c = transcript.clone();
        let out_dir_c = out_dir.clone();
        let src_c = src.to_path_buf();
        let render = tauri::async_runtime::spawn_blocking(move || {
            let tmap = TranscriptMap::single(transcript_c);
            render_edit(&app_c, &sub_job, &src_c, &plan, &tmap, &out_dir_c, prefer_hw)
        })
        .await
        .map_err(|e| e.to_string());

        match render {
            Ok(Ok(path)) => results.push(ShortResult {
                title: spec.title.clone(),
                hook: spec.hook.clone(),
                path: path.to_string_lossy().to_string(),
                duration_s: dur,
            }),
            Ok(Err(e)) | Err(e) => {
                // A cancel SIGKILLs the sub-render, which surfaces as a render error — swallow it
                // and stop the loop rather than reporting a spurious failure.
                if is_pipeline_cancelled(job_id) {
                    break;
                }
                eprintln!("[shorts] render of '{}' failed: {e}", spec.title);
                last_err = Some(e);
            }
        }
    }

    clear_pipeline_cancel(job_id);
    emit_stage(results.len(), total, "", "done");
    let _ = app.emit("content-changed", ());

    if results.is_empty() {
        if let Some(e) = last_err {
            return Err(format!("no shorts rendered; last error: {e}"));
        }
        // No error and no results ⇒ cancelled before any short finished.
    }
    Ok(results)
}

// ---------- multi-clip compilation (combine several clips, keep the good parts) ----------

/// One input clip fed to the compilation planner. Built by `run_compile_pipeline` after it
/// transcribes + analyzes each clip. `path` is the absolute clip path (also the segment
/// `source` the planner keys off of); `transcript_segments` is the segment-level transcript
/// (empty if transcription failed — the clip still participates, with a note to the LLM).
#[derive(Clone, Debug)]
pub struct ClipInfo {
    pub path: String,
    pub duration_s: f64,
    pub transcript_segments: Vec<TranscriptSegment>,
    pub filler_count: usize,
    /// Fraction of the clip that is detected silence/dead-air, 0.0–1.0.
    pub silence_ratio: f64,
}

/// The LLM's plan for one compiled video. `segments` reference input clips by their absolute
/// path via `Segment.source`: the planner emits an explicit `source` on EVERY segment (naming
/// the clip it comes from). Before rendering, the pipeline rewrites segments whose source is
/// the FIRST clip to `source: None` — that clip is the render's MAIN input (`inputs[0]`) and
/// its dims/fps are the normalization reference; the rest stay as inserts.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct CompilationSpec {
    pub title: String,
    /// "takes" (retakes aligned into sections, best take per section) or "montage" (distinct
    /// strong moments across clips). Reported by the model; also echoed to the caller.
    pub mode_detected: String,
    /// Ordered segments in final playback order. `source` = an input clip path.
    pub segments: Vec<Segment>,
    /// The model's short rationale (surfaced for transparency).
    pub notes: String,
}

/// One rendered compilation returned to the caller.
#[derive(Serialize, Clone, Debug)]
pub struct CompileResult {
    pub title: String,
    pub mode_detected: String,
    pub path: String,
    pub duration_s: f64,
    pub notes: String,
}

/// Pipeline stage event so the UI can show which phase a compile job is in.
#[derive(Serialize, Clone)]
struct CompileProgress {
    job_id: String,
    /// 1-based index of the clip currently being processed (0 during planning/rendering).
    current: usize,
    total: usize,
    /// "transcribing" | "planning" | "rendering" | "done".
    stage: String,
}

/// Validate + normalize an LLM-produced compilation spec against the input clips. Pure so it's
/// unit-testable without an LLM. Rules:
/// - every segment must carry a `source` that is EXACTLY one of the input clip paths (a segment
///   with no/unknown source is dropped — we never guess which clip it meant);
/// - each segment is clamped into `[0, that clip's duration]` and dropped if under 0.5s;
/// - segments keep the model's given order (narrative/flow order matters — we do NOT sort);
/// - at most 100 segments;
/// - when a positive `target_s` is given, the total kept duration must land within 0.5×–2× of
///   it (else the whole spec is rejected — a re-prompt is cheaper than a wrong-length render);
/// - title trimmed to ≤80 chars; `mode_detected` normalized to "takes"/"montage" (default
///   "montage" if the model returns something else).
pub(crate) fn sanitize_compilation_spec(
    mut spec: CompilationSpec,
    clip_durations: &[(String, f64)],
    target_s: Option<f64>,
) -> Result<CompilationSpec, String> {
    if clip_durations.is_empty() {
        return Err("no input clips to compile".to_string());
    }
    let dur_of = |src: &str| -> Option<f64> {
        clip_durations
            .iter()
            .find(|(p, _)| p.trim() == src.trim())
            .map(|(_, d)| *d)
    };

    let mut segs: Vec<Segment> = Vec::new();
    for s in &spec.segments {
        let src = match &s.source {
            Some(src) if !src.trim().is_empty() => src.trim().to_string(),
            _ => continue, // no source ⇒ we can't tell which clip; drop it
        };
        let clip_dur = match dur_of(&src) {
            Some(d) => d,
            None => continue, // unknown clip path ⇒ drop (never a workspace escape / guess)
        };
        let start = s.start.max(0.0);
        let end = s.end.min(clip_dur);
        if end - start < 0.5 {
            continue;
        }
        segs.push(Segment { start, end, source: Some(src) });
        if segs.len() >= 100 {
            break;
        }
    }
    if segs.is_empty() {
        return Err(
            "no usable compilation segments after validation (every segment needs a valid clip \
             source and a ≥0.5s range inside that clip)"
                .to_string(),
        );
    }

    if let Some(target) = target_s.filter(|t| *t > 0.0) {
        let total: f64 = segs.iter().map(|s| s.end - s.start).sum();
        let (lo, hi) = (0.5 * target, 2.0 * target);
        if total < lo || total > hi {
            return Err(format!(
                "compilation total is {total:.0}s but should be within {lo:.0}–{hi:.0}s of the \
                 {target:.0}s target — pick more/fewer or shorter/longer moments"
            ));
        }
    }

    spec.segments = segs;
    spec.title = {
        let t: String = spec.title.trim().chars().take(80).collect();
        if t.is_empty() { "Compilation".to_string() } else { t }
    };
    spec.mode_detected = match spec.mode_detected.trim().to_lowercase().as_str() {
        "takes" | "take" | "retakes" => "takes".to_string(),
        _ => "montage".to_string(),
    };
    Ok(spec)
}

/// The full multi-clip compilation pipeline: transcribe + analyze every clip → LLM planning
/// (retakes vs montage) → ONE stitched render into the Content library. Mirrors
/// `run_shorts_pipeline`.
///
/// Emits `compile-progress` (`{job_id, current, total, stage}`) at each phase, and the single
/// render's `video-render-progress` keyed by the compile `job_id` itself. Cancellation is
/// cooperative: `request_cancel_compile(job_id)` flags the job (checked between clips + before
/// the render) and SIGKILLs the active render. A clip that fails transcription still
/// participates (empty transcript + a note to the planner).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_compile_pipeline(
    app: &AppHandle,
    state: &AppState,
    paths: &[PathBuf],
    brief: Option<&str>,
    mode: &str,
    target_duration_s: Option<f64>,
    captions: bool,
    skip_polish: bool,
    job_id: &str,
) -> Result<CompileResult, String> {
    clear_pipeline_cancel(job_id); // clear any stale flag from a previous job reusing this id
    if paths.len() < 2 {
        return Err("compilation needs at least 2 clips".to_string());
    }
    if paths.len() > 10 {
        return Err("compilation takes at most 10 clips".to_string());
    }

    let emit_stage = |current: usize, total: usize, stage: &str| {
        let _ = app.emit(
            "compile-progress",
            CompileProgress {
                job_id: job_id.to_string(),
                current,
                total,
                stage: stage.to_string(),
            },
        );
    };

    // ---- per-clip: transcribe + analyze, building ClipInfo + a per-source transcript map ----
    let total = paths.len();
    let mut clips: Vec<ClipInfo> = Vec::new();
    let mut transcripts = TranscriptMap::default();
    for (i, path) in paths.iter().enumerate() {
        if is_pipeline_cancelled(job_id) {
            clear_pipeline_cancel(job_id);
            emit_stage(0, total, "done");
            return Err("compilation cancelled".to_string());
        }
        emit_stage(i + 1, total, "transcribing");
        let path_str = path.to_string_lossy().to_string();
        let probe = {
            let p = path.clone();
            tauri::async_runtime::spawn_blocking(move || probe_video(&p))
                .await
                .map_err(|e| e.to_string())??
        };
        // A clip that can't be transcribed still participates with an empty transcript.
        let transcript = match transcribe_video(state, path).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[compile] transcription of '{path_str}' failed: {e}");
                Transcript::default()
            }
        };
        // First clip is the render's MAIN source (keyed None); the rest are inserts keyed by path.
        if i == 0 {
            transcripts.insert(None, transcript.clone());
        } else {
            transcripts.insert(Some(&path_str), transcript.clone());
        }

        let filler_count = detect_filler_words(&transcript).len();
        let silence_ratio = {
            let p = path.clone();
            let sil = tauri::async_runtime::spawn_blocking(move || detect_silences(&p, -35.0, 0.6))
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            let total_sil: f64 = sil.iter().map(|s| (s.end - s.start).max(0.0)).sum();
            if probe.duration_s > 0.0 { (total_sil / probe.duration_s).clamp(0.0, 1.0) } else { 0.0 }
        };
        clips.push(ClipInfo {
            path: path_str,
            duration_s: probe.duration_s,
            transcript_segments: transcript.segments,
            filler_count,
            silence_ratio,
        });
    }

    if is_pipeline_cancelled(job_id) {
        clear_pipeline_cancel(job_id);
        emit_stage(0, total, "done");
        return Err("compilation cancelled".to_string());
    }

    // ---- plan ----
    emit_stage(0, total, "planning");
    let spec = crate::llm::generate_compilation_specs(state, &clips, brief, mode, target_duration_s).await?;

    if is_pipeline_cancelled(job_id) {
        clear_pipeline_cancel(job_id);
        emit_stage(0, total, "done");
        return Err("compilation cancelled".to_string());
    }

    // ---- build ONE EditPlan ----
    // The planner names an explicit clip `source` on every segment. The render's MAIN input is
    // clip 0, so rewrite clip-0 segments to `source: None` (its captions are keyed None too);
    // the rest stay as inserts (matched to their transcript by path in the transcript map).
    let main_path = clips[0].path.clone();
    let segments: Vec<Segment> = spec
        .segments
        .iter()
        .map(|s| {
            let is_main = s.source.as_deref().map(|p| p.trim() == main_path).unwrap_or(false);
            Segment {
                start: s.start,
                end: s.end,
                source: if is_main { None } else { s.source.clone() },
            }
        })
        .collect();
    // Only burn captions if at least one clip actually transcribed — otherwise a fully-silent
    // set would hard-fail the render on "captions requested but no transcript". Degrade quietly.
    let want_captions = captions && transcripts.has_captions();
    let plan = EditPlan {
        segments,
        captions: want_captions,
        caption_style: if want_captions { Some("karaoke".to_string()) } else { None },
        normalize_audio: !skip_polish,
        fade: if skip_polish { None } else { Some(0.3) },
        output_name: Some(sanitize_stem(&spec.title)),
        ..Default::default()
    };
    let expected_dur: f64 = spec.segments.iter().map(|s| (s.end - s.start).max(0.0)).sum();

    // ---- render ----
    emit_stage(0, total, "rendering");
    let prefer_hw = hw_encode_preferred(state);
    let out_dir = crate::tools::content_dir(app)?;
    let main_src = PathBuf::from(&main_path);
    let app_c = app.clone();
    let job = job_id.to_string();
    let out_dir_c = out_dir.clone();
    let render = tauri::async_runtime::spawn_blocking(move || {
        render_edit(&app_c, &job, &main_src, &plan, &transcripts, &out_dir_c, prefer_hw)
    })
    .await
    .map_err(|e| e.to_string());

    clear_pipeline_cancel(job_id);
    let path = match render {
        Ok(Ok(p)) => p,
        Ok(Err(e)) | Err(e) => {
            if is_pipeline_cancelled(job_id) {
                emit_stage(0, total, "done");
                return Err("compilation cancelled".to_string());
            }
            emit_stage(0, total, "done");
            return Err(format!("compilation render failed: {e}"));
        }
    };

    emit_stage(0, total, "done");
    let _ = app.emit("content-changed", ());
    Ok(CompileResult {
        title: spec.title,
        mode_detected: spec.mode_detected,
        path: path.to_string_lossy().to_string(),
        duration_s: expected_dur,
        notes: spec.notes,
    })
}

// ---------- thumbnails ----------

/// Evenly-spaced small JPEG thumbnails as data URLs (for the Studio filmstrip).
pub(crate) fn thumbnails(path: &Path, count: u32) -> Result<Vec<String>, String> {
    let probe = probe_video(path)?;
    let count = count.clamp(1, 20);
    let ffmpeg = ffmpeg_path()?;
    let mut out = Vec::new();
    for i in 0..count {
        let t = probe.duration_s * (i as f64 + 0.5) / count as f64;
        let result = Command::new(&ffmpeg)
            .args(["-hide_banner", "-nostdin", "-ss", &format!("{t:.3}"), "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", "scale=180:-2", "-f", "image2", "-c:v", "mjpeg", "pipe:1"])
            .output()
            .map_err(|e| format!("ffmpeg failed to start: {e}"))?;
        if result.status.success() && !result.stdout.is_empty() {
            out.push(format!("data:image/jpeg;base64,{}", B64.encode(&result.stdout)));
        }
    }
    Ok(out)
}

// ---------- transcription (word/segment timestamps) ----------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TranscriptSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TranscriptWord {
    pub start: f64,
    pub end: f64,
    pub word: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Transcript {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub words: Vec<TranscriptWord>,
}

/// Extract the audio track as a small mono MP3 (keeps long videos under API upload caps).
fn extract_audio_mp3(src: &Path, dest: &Path) -> Result<(), String> {
    let out = Command::new(ffmpeg_path()?)
        .args(["-y", "-hide_banner", "-nostdin", "-i"])
        .arg(src)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "libmp3lame", "-b:a", "64k"])
        .arg(dest)
        .output()
        .map_err(|e| format!("ffmpeg failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "audio extraction failed: {}",
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
        ));
    }
    Ok(())
}

/// Locate the compiled Swift on-device transcriber (see speech/transcriber.swift).
/// Bundled builds have it next to the app binary (tauri `externalBin` sidecar); dev and
/// `cargo test` runs fall back to the build.rs output in src-tauri/binaries/.
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

/// Run the on-device (Apple Speech) helper on an extracted audio file. Blocking —
/// the first use per locale downloads Apple's speech model, which can take a while.
/// Also used by `transcribe_wav` (chat voice input) via `commands::voice`.
pub(crate) fn transcribe_local(audio: &Path) -> Result<Transcript, String> {
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
    let t: Transcript = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("on-device transcriber returned invalid JSON: {e}"))?;
    if t.text.is_empty() && t.segments.is_empty() {
        return Err("on-device transcriber produced an empty transcript".to_string());
    }
    Ok(t)
}

/// Transcribe a video's audio with segment + word timestamps. When the
/// `video/local_transcription` setting is "1", Apple's on-device speech engine is tried
/// first (SpeechAnalyzer on macOS 26, SFSpeechRecognizer otherwise — no network, no
/// key); any failure falls through to the cloud whisper path.
pub(crate) async fn transcribe_video(state: &AppState, src: &Path) -> Result<Transcript, String> {
    let probe = probe_video(src)?;
    if !probe.has_audio {
        return Err("video has no audio track to transcribe".to_string());
    }

    let tmp = std::env::temp_dir().join(format!(
        "muninn-transcribe-{}.mp3",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
    ));
    let src_owned = src.to_path_buf();
    let tmp_clone = tmp.clone();
    tauri::async_runtime::spawn_blocking(move || extract_audio_mp3(&src_owned, &tmp_clone))
        .await
        .map_err(|e| e.to_string())??;

    let mut local_err: Option<String> = None;
    let use_local = get_app_setting_value(state, "video/local_transcription")?.as_deref() == Some("1");
    if use_local {
        let tmp_clone = tmp.clone();
        let local = tauri::async_runtime::spawn_blocking(move || transcribe_local(&tmp_clone))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        match local {
            Ok(t) => {
                let _ = std::fs::remove_file(&tmp);
                return Ok(t);
            }
            Err(e) => local_err = Some(e),
        }
    }

    let result = transcribe_cloud(state, &tmp).await;
    let _ = std::fs::remove_file(&tmp);
    match (result, local_err) {
        (Ok(t), _) => Ok(t),
        (Err(cloud_err), Some(local_err)) => Err(format!(
            "on-device transcription failed ({local_err}); cloud fallback also failed: {cloud_err}"
        )),
        (Err(cloud_err), None) => Err(cloud_err),
    }
}

/// Cloud whisper on an extracted audio file (Groq's whisper-large-v3-turbo when a Groq
/// key is configured, else OpenAI whisper-1 — same key precedence as `transcribe_wav`).
async fn transcribe_cloud(state: &AppState, audio: &Path) -> Result<Transcript, String> {
    // Provider precedence is governed by the `stt/provider` setting (auto/groq/openai),
    // shared with `transcribe_wav` so both cloud paths agree.
    let (api_key, model, endpoint) = crate::llm::resolve_stt(state)?;

    let audio_bytes = std::fs::read(audio).map_err(|e| e.to_string())?;
    if audio_bytes.len() > 24 * 1024 * 1024 {
        return Err("extracted audio exceeds the 25 MB transcription upload limit — trim the video first".to_string());
    }

    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.mp3")
        .mime_str("audio/mpeg")
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new()
        .text("model", model)
        .text("response_format", "verbose_json")
        .text("timestamp_granularities[]", "segment")
        .text("timestamp_granularities[]", "word")
        .part("file", part);

    let resp = state
        .http
        .post(endpoint)
        .bearer_auth(&api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("transcription request failed: {e}"))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("transcription API error: {body}"));
    }
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;

    let mut t = Transcript {
        text: json["text"].as_str().unwrap_or("").trim().to_string(),
        ..Default::default()
    };
    if let Some(segs) = json["segments"].as_array() {
        for s in segs {
            let text = s["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                continue;
            }
            t.segments.push(TranscriptSegment {
                start: s["start"].as_f64().unwrap_or(0.0),
                end: s["end"].as_f64().unwrap_or(0.0),
                text,
            });
        }
    }
    if let Some(words) = json["words"].as_array() {
        for w in words {
            let word = w["word"].as_str().unwrap_or("").trim().to_string();
            if word.is_empty() {
                continue;
            }
            t.words.push(TranscriptWord {
                start: w["start"].as_f64().unwrap_or(0.0),
                end: w["end"].as_f64().unwrap_or(0.0),
                word,
            });
        }
    }
    Ok(t)
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-main-source shim so the many pre-multi-source tests keep their old call shape.
    fn build_args_single(
        src: &Path,
        plan: &EditPlan,
        probe: &VideoProbe,
        font: &str,
        text_files: &[PathBuf],
        ass_file: Option<&Path>,
        encoder: VideoEncoder,
        out: &Path,
    ) -> Result<Vec<String>, String> {
        let inputs = vec![SourceInput { path: src.to_path_buf(), probe: probe.clone(), is_image: false }];
        build_render_args(&inputs, plan, font, text_files, ass_file, None, encoder, out)
    }

    /// A minimal image-insert probe (dims only; no duration/audio, like ffprobe on a PNG).
    fn image_probe(path: &str, w: u32, h: u32) -> VideoProbe {
        VideoProbe {
            path: path.into(),
            duration_s: 0.0,
            width: w,
            height: h,
            fps: 0.0,
            has_audio: false,
            video_codec: "png".into(),
            size_bytes: 1,
        }
    }

    fn probe_16x9() -> VideoProbe {
        VideoProbe {
            path: "/tmp/x.mp4".into(),
            duration_s: 60.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            has_audio: true,
            video_codec: "h264".into(),
            size_bytes: 1,
        }
    }

    #[test]
    fn parses_silencedetect_output() {
        let stderr = "\
[silencedetect @ 0x1] silence_start: 4.017007
[silencedetect @ 0x1] silence_end: 7.012426 | silence_duration: 2.99542
[silencedetect @ 0x1] silence_start: 55.5
";
        let ranges = parse_silences(stderr, 60.0);
        assert_eq!(ranges.len(), 2);
        assert!((ranges[0].start - 4.017007).abs() < 1e-6);
        assert!((ranges[0].end - 7.012426).abs() < 1e-6);
        assert!((ranges[1].end - 60.0).abs() < 1e-6); // unclosed start runs to EOF
    }

    #[test]
    fn keep_segments_inverts_silences_with_padding() {
        let silences = vec![SilenceRange { start: 10.0, end: 20.0 }];
        let segs = keep_segments(60.0, &silences, 0.15);
        assert_eq!(segs.len(), 2);
        assert!((segs[0].end - 10.15).abs() < 1e-6);
        assert!((segs[1].start - 19.85).abs() < 1e-6);
        assert!((segs[1].end - 60.0).abs() < 1e-6);
    }

    #[test]
    fn atempo_chains_extreme_speeds() {
        assert_eq!(atempo_chain(1.5), "atempo=1.500000");
        assert_eq!(atempo_chain(3.0), "atempo=2.0,atempo=1.500000");
        assert_eq!(atempo_chain(0.3), "atempo=0.5,atempo=0.600000");
    }

    #[test]
    fn validate_rejects_bad_plans() {
        let probe = probe_16x9();
        let mut p = EditPlan { segments: vec![Segment::keep(5.0, 4.0)], ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut p = EditPlan { filter: Some("sepia-dreams".into()), ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut p = EditPlan { speed: Some(9.0), ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut p = EditPlan { aspect: Some("9x16".into()), ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut ok = EditPlan {
            segments: vec![Segment::keep(0.0, 10.0)],
            filter: Some("punchy".into()),
            speed: Some(1.5),
            aspect: Some("9:16".into()),
            ..Default::default()
        };
        assert!(ok.validate(&probe).is_ok());
    }

    #[test]
    fn build_args_full_plan_shape() {
        let probe = probe_16x9();
        let plan = EditPlan {
            segments: vec![Segment::keep(1.0, 4.0), Segment::keep(7.0, 11.0)],
            filter: Some("punchy".into()),
            texts: vec![TextOverlay { text: "Hi".into(), start: 0.5, end: 3.0, position: None, size: None }],
            aspect: Some("9:16".into()),
            speed: Some(2.0),
            mute: false,
            output_name: None,
            captions: false,
            caption_style: None,
            ..Default::default()
        };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"),
            &plan,
            &probe,
            "/System/Library/Fonts/Helvetica.ttc",
            &[PathBuf::from("/tmp/t0.txt")],
            None,
            VideoEncoder::Libx264,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("concat=n=2:v=1:a=1"));
        assert!(graph.contains("setpts=PTS/2.000000"));
        assert!(graph.contains("atempo=2.000000"));
        assert!(graph.contains("crop=min(iw\\,ih*9/16)"));
        assert!(graph.contains("textfile=/tmp/t0.txt"));
        assert!(args.iter().any(|a| a == "[vout]"));
        assert!(args.iter().any(|a| a == "[aout]"));
        assert!((plan.output_duration(&probe) - 3.5).abs() < 1e-6); // (3+4)/2
    }

    /// Full pipeline against real ffmpeg — needs ffmpeg installed, so it's ignored in CI.
    /// Run locally with: cargo test --lib video -- --ignored
    #[test]
    #[ignore]
    fn render_args_run_end_to_end_with_real_ffmpeg() {
        let dir = std::env::temp_dir().join("muninn-video-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let status = Command::new(ffmpeg_path().unwrap())
            .args(["-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=10",
                   "-f", "lavfi", "-i", "sine=frequency=440:duration=10",
                   "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());

        let probe = probe_video(&src).unwrap();
        assert!((probe.duration_s - 10.0).abs() < 0.2);
        assert!(probe.has_audio);

        let text_file = dir.join("t0.txt");
        std::fs::write(&text_file, "Hello: 100% 'quoted'\\ text").unwrap(); // hostile chars
        let mut plan = EditPlan {
            segments: vec![Segment::keep(1.0, 3.0), Segment::keep(5.0, 8.0)],
            filter: Some("cinematic".into()),
            texts: vec![TextOverlay { text: "x".into(), start: 0.5, end: 2.0, position: Some("top".into()), size: Some(36) }],
            aspect: Some("9:16".into()),
            speed: Some(1.25),
            mute: false,
            output_name: None,
            captions: false,
            caption_style: None,
            normalize_audio: true,
            denoise: true,
            fade: Some(0.5),
            smart_reframe: false,
        };
        plan.validate(&probe).unwrap();
        let out = dir.join("out.mp4");
        let args =
            build_args_single(&src, &plan, &probe, font_path().unwrap(), &[text_file], None, VideoEncoder::Libx264, &out)
                .unwrap();
        let result = Command::new(ffmpeg_path().unwrap()).args(&args).output().unwrap();
        assert!(
            result.status.success(),
            "ffmpeg failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let out_probe = probe_video(&out).unwrap();
        assert!((out_probe.duration_s - 4.0).abs() < 0.3, "expected ~4s, got {}", out_probe.duration_s); // (2+3)/1.25
        // 9:16 crop, allowing for even-pixel rounding of the crop width.
        let ratio = out_probe.width as f64 / out_probe.height as f64;
        assert!((ratio - 9.0 / 16.0).abs() < 0.02, "expected ~9:16, got {}x{}", out_probe.width, out_probe.height);

        // Burned captions through libass: render a karaoke-styled ASS and confirm ffmpeg
        // accepts the subtitles= filter after the crop stage.
        let transcript = transcript_fixture();
        let cap_plan = EditPlan { captions: true, caption_style: Some("karaoke".into()), aspect: Some("9:16".into()), ..Default::default() };
        let ass_path = dir.join("captions.ass");
        std::fs::write(&ass_path, build_ass(&transcript, &cap_plan, &probe)).unwrap();
        let cap_out = dir.join("captioned.mp4");
        let cap_args = build_args_single(
            &src,
            &cap_plan,
            &probe,
            font_path().unwrap(),
            &[],
            Some(&ass_path),
            VideoEncoder::Libx264,
            &cap_out,
        )
        .unwrap();
        let cap_result = Command::new(ffmpeg_path().unwrap()).args(&cap_args).output().unwrap();
        assert!(
            cap_result.status.success(),
            "captioned ffmpeg render failed:\n{}",
            String::from_utf8_lossy(&cap_result.stderr)
        );
        assert!(probe_video(&cap_out).unwrap().duration_s > 5.0);

        // Multi-source: main clip + a still-image title card insert. Generate a solid-color PNG
        // (the same lavfi trick the src clip uses) and splice it in as a 2s silent card.
        let card = dir.join("card.png");
        let card_status = Command::new(ffmpeg_path().unwrap())
            .args(["-y", "-f", "lavfi", "-i", "color=c=navy:s=400x300", "-frames:v", "1", "-update", "1"])
            .arg(&card)
            .status()
            .unwrap();
        assert!(card_status.success());
        let card_probe = probe_video(&card).unwrap();
        assert!(card_probe.width == 400 && card_probe.height == 300);

        let multi_inputs = vec![
            SourceInput { path: src.clone(), probe: probe.clone(), is_image: false },
            SourceInput { path: card.clone(), probe: card_probe, is_image: true },
        ];
        // 2s card, then keep 1..4 (3s) of the main → ~5s output.
        let mut multi_plan = EditPlan {
            segments: vec![
                Segment { start: 0.0, end: 2.0, source: Some(card.to_string_lossy().to_string()) },
                Segment::keep(1.0, 4.0),
            ],
            ..Default::default()
        };
        multi_plan.validate(&probe).unwrap();
        let multi_out = dir.join("multi.mp4");
        let multi_args = build_render_args(
            &multi_inputs,
            &multi_plan,
            font_path().unwrap(),
            &[],
            None,
            None,
            VideoEncoder::Libx264,
            &multi_out,
        )
        .unwrap();
        let multi_result = Command::new(ffmpeg_path().unwrap()).args(&multi_args).output().unwrap();
        assert!(
            multi_result.status.success(),
            "multi-source (image insert) ffmpeg render failed:\n{}",
            String::from_utf8_lossy(&multi_result.stderr)
        );
        let multi_probe = probe_video(&multi_out).unwrap();
        assert!((multi_probe.duration_s - 5.0).abs() < 0.4, "expected ~5s, got {}", multi_probe.duration_s);
        // The image insert was scaled/cropped to the MAIN dims (640x360) for concat.
        assert_eq!((multi_probe.width, multi_probe.height), (640, 360));
        assert!(multi_probe.has_audio, "silent card + main audio → output keeps an audio track");

        // Compile-style two-source render WITH per-source captions: main clip + a second real
        // b-roll clip, each with its own transcript, and burned cross-source subtitles. Confirms
        // ffmpeg accepts the multi-source subtitles= filter driven by build_ass_multi.
        let clip_b = dir.join("clipB.mp4");
        let cb_status = Command::new(ffmpeg_path().unwrap())
            .args(["-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=6",
                   "-f", "lavfi", "-i", "sine=frequency=660:duration=6",
                   "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest"])
            .arg(&clip_b)
            .status()
            .unwrap();
        assert!(cb_status.success());
        let clip_b_probe = probe_video(&clip_b).unwrap();

        let compile_plan = EditPlan {
            // MAIN 1..3 (2s), then clipB 0..2 (2s) → ~4s output.
            segments: vec![
                Segment::keep(1.0, 3.0),
                Segment { start: 0.0, end: 2.0, source: Some(clip_b.to_string_lossy().to_string()) },
            ],
            captions: true,
            caption_style: Some("karaoke".into()),
            normalize_audio: true,
            fade: Some(0.3),
            ..Default::default()
        };
        let mut compile_tmap = TranscriptMap::default();
        compile_tmap.insert(None, Transcript {
            text: "main words here".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 3.0, text: "main words here".into() }],
            words: vec![
                TranscriptWord { start: 1.2, end: 1.6, word: "main".into() },
                TranscriptWord { start: 1.7, end: 2.2, word: "words".into() },
            ],
        });
        compile_tmap.insert(Some(&clip_b.to_string_lossy().to_string()), Transcript {
            text: "broll caption".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 2.0, text: "broll caption".into() }],
            words: vec![
                TranscriptWord { start: 0.3, end: 0.7, word: "broll".into() },
                TranscriptWord { start: 0.8, end: 1.3, word: "caption".into() },
            ],
        });
        let compile_ass = dir.join("compile-captions.ass");
        std::fs::write(&compile_ass, build_ass_multi(&compile_tmap, &compile_plan, &probe)).unwrap();
        // Both sources' words appear (karaoke \k tags sit between them), and clipB's land past
        // the main's 2s slot on the output: main line at output 0.2, clipB line at 2.3.
        let ass_doc = std::fs::read_to_string(&compile_ass).unwrap();
        assert!(ass_doc.contains("main") && ass_doc.contains("broll") && ass_doc.contains("caption"), "ass:\n{ass_doc}");
        assert!(ass_doc.contains("Dialogue: 0,0:00:00.20,"), "main caption at output 0.2:\n{ass_doc}");
        assert!(ass_doc.contains("Dialogue: 0,0:00:02.30,"), "clipB caption after main's slot:\n{ass_doc}");

        let compile_inputs = vec![
            SourceInput { path: src.clone(), probe: probe.clone(), is_image: false },
            SourceInput { path: clip_b.clone(), probe: clip_b_probe, is_image: false },
        ];
        let compile_out = dir.join("compiled.mp4");
        let compile_args = build_render_args(
            &compile_inputs,
            &compile_plan,
            font_path().unwrap(),
            &[],
            Some(&compile_ass),
            None,
            VideoEncoder::Libx264,
            &compile_out,
        )
        .unwrap();
        let compile_result = Command::new(ffmpeg_path().unwrap()).args(&compile_args).output().unwrap();
        assert!(
            compile_result.status.success(),
            "compile-style captioned two-source render failed:\n{}",
            String::from_utf8_lossy(&compile_result.stderr)
        );
        let compile_probe = probe_video(&compile_out).unwrap();
        assert!((compile_probe.duration_s - 4.0).abs() < 0.4, "expected ~4s, got {}", compile_probe.duration_s);
        assert!(compile_probe.has_audio);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On-device transcription against the real Swift helper — needs the compiled
    /// sidecar plus `say`/ffmpeg, so it's ignored in CI. Run locally with:
    /// cargo test --lib video -- --ignored
    #[test]
    #[ignore]
    fn local_transcriber_end_to_end() {
        let dir = std::env::temp_dir().join("muninn-transcriber-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let aiff = dir.join("speech.aiff");
        let status = Command::new("/usr/bin/say")
            .args(["-o"])
            .arg(&aiff)
            .arg("The quick brown fox jumps over the lazy dog.")
            .status()
            .unwrap();
        assert!(status.success());
        let mp3 = dir.join("speech.mp3");
        extract_audio_mp3(&aiff, &mp3).unwrap();

        let t = transcribe_local(&mp3).unwrap();
        assert!(t.text.to_lowercase().contains("fox"), "unexpected transcript: {}", t.text);
        assert!(!t.segments.is_empty());
        assert!(!t.words.is_empty());
        assert!(t.words.iter().all(|w| w.end >= w.start));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_args_muted_has_no_audio_chain() {
        let probe = probe_16x9();
        // mute=true must skip ALL audio work — denoise, normalize, and fade included —
        // since there's no audio track in the output to apply them to.
        let plan = EditPlan {
            mute: true,
            denoise: true,
            normalize_audio: true,
            fade: Some(1.0),
            segments: vec![Segment::keep(0.0, 10.0), Segment::keep(20.0, 30.0)],
            ..Default::default()
        };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"),
            &plan,
            &probe,
            "/tmp/f.ttc",
            &[],
            None,
            VideoEncoder::Libx264,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("concat=n=2:v=1:a=0"));
        assert!(!graph.contains("[aout]"));
        assert!(!graph.contains("afftdn"));
        assert!(!graph.contains("loudnorm"));
        assert!(!graph.contains("afade"));
        // The video fade still applies even though audio is muted.
        assert!(graph.contains("fade=t=in"));
        assert!(args.iter().any(|a| a == "-an"));
    }

    #[test]
    fn build_args_normalize_audio_is_last_filter() {
        let probe = probe_16x9();
        let plan = EditPlan { normalize_audio: true, speed: Some(1.5), ..Default::default() };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"),
            &plan,
            &probe,
            "/tmp/f.ttc",
            &[],
            None,
            VideoEncoder::Libx264,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("loudnorm=I=-14:TP=-1.5:LRA=11"));
        // loudnorm must be the last stage of the [aout] chain, after atempo.
        let aout_chain = graph.split(";[ca]").nth(1).expect("audio post-chain present");
        let atempo_at = aout_chain.find("atempo=").expect("atempo present");
        let loudnorm_at = aout_chain.find("loudnorm=").expect("loudnorm present");
        assert!(loudnorm_at > atempo_at, "loudnorm must come after atempo: {aout_chain}");
        assert!(aout_chain.trim_end_matches("[aout]").ends_with("LRA=11"), "loudnorm must be last: {aout_chain}");
    }

    #[test]
    fn build_args_denoise_runs_before_atempo_and_loudnorm() {
        let probe = probe_16x9();
        let plan = EditPlan { denoise: true, normalize_audio: true, speed: Some(2.0), ..Default::default() };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"),
            &plan,
            &probe,
            "/tmp/f.ttc",
            &[],
            None,
            VideoEncoder::Libx264,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("afftdn=nf=-25"));
        let denoise_at = graph.find("afftdn=").unwrap();
        let atempo_at = graph.find("atempo=").unwrap();
        let loudnorm_at = graph.find("loudnorm=").unwrap();
        assert!(denoise_at < atempo_at, "denoise must run before atempo");
        assert!(denoise_at < loudnorm_at, "denoise must run before loudnorm");
    }

    #[test]
    fn build_args_fade_timing_and_both_streams() {
        let probe = probe_16x9();
        // Single 8s segment at 2x speed → 4s output; a 1s fade should fade out starting at 3s.
        let plan = EditPlan {
            segments: vec![Segment::keep(0.0, 8.0)],
            speed: Some(2.0),
            fade: Some(1.0),
            ..Default::default()
        };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"),
            &plan,
            &probe,
            "/tmp/f.ttc",
            &[],
            None,
            VideoEncoder::Libx264,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        // Video fade on the [cv] chain.
        assert!(graph.contains("fade=t=in:st=0:d=1.0000"));
        assert!(graph.contains("fade=t=out:st=3.0000:d=1.0000"), "graph: {graph}");
        // Audio fade on the [ca] chain, same timing.
        assert!(graph.contains("afade=t=in:st=0:d=1.0000"));
        assert!(graph.contains("afade=t=out:st=3.0000:d=1.0000"), "graph: {graph}");
    }

    #[test]
    fn validate_rejects_out_of_range_fade() {
        let probe = probe_16x9();
        let mut too_short = EditPlan { fade: Some(0.05), ..Default::default() };
        assert!(too_short.validate(&probe).is_err());
        let mut too_long = EditPlan { fade: Some(6.0), ..Default::default() };
        assert!(too_long.validate(&probe).is_err());
        let mut ok = EditPlan { fade: Some(1.5), ..Default::default() };
        assert!(ok.validate(&probe).is_ok());
    }

    #[test]
    fn build_args_microfades_only_with_two_or_more_segments() {
        let probe = probe_16x9();
        // Single segment: no cut, so no de-click micro-fades should appear.
        let one_seg = EditPlan { segments: vec![Segment::keep(1.0, 10.0)], ..Default::default() };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"), &one_seg, &probe, "/tmp/f.ttc", &[], None, VideoEncoder::Libx264, Path::new("/tmp/out.mp4"),
        ).unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(!graph.contains("afade"), "no cuts, no micro-fades expected: {graph}");

        // Two segments: a concat happens, so each per-segment audio clip gets a ~20ms
        // in/out afade to avoid an audible click at the cut.
        let two_segs = EditPlan {
            segments: vec![Segment::keep(1.0, 5.0), Segment::keep(10.0, 15.0)],
            ..Default::default()
        };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"), &two_segs, &probe, "/tmp/f.ttc", &[], None, VideoEncoder::Libx264, Path::new("/tmp/out.mp4"),
        ).unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        let microfade_count = graph.matches("afade=t=in:st=0:d=0.0200").count();
        assert_eq!(microfade_count, 2, "expected one in-fade per segment: {graph}");
        let microfade_out_count = graph.matches(",afade=t=out:st=").count();
        assert_eq!(microfade_out_count, 2, "expected one out-fade per segment: {graph}");
    }

    fn transcript_fixture() -> Transcript {
        Transcript {
            text: "one two three four five six".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 6.0, text: "one two three four five six".into() }],
            words: vec![
                TranscriptWord { start: 0.0, end: 0.5, word: "one".into() },
                TranscriptWord { start: 0.6, end: 1.0, word: "two".into() },
                TranscriptWord { start: 1.1, end: 1.5, word: "three".into() },
                TranscriptWord { start: 4.0, end: 4.5, word: "four".into() },
                TranscriptWord { start: 4.6, end: 5.0, word: "five".into() },
                TranscriptWord { start: 5.1, end: 5.5, word: "six".into() },
            ],
        }
    }

    #[test]
    fn map_interval_whole_video_identity_and_speed() {
        let segs = vec![Segment::keep(0.0, 10.0)];
        // speed 1: identity
        let (s, e, i) = map_interval_to_output(2.0, 3.0, &segs, 1.0).unwrap();
        assert!((s - 2.0).abs() < 1e-9 && (e - 3.0).abs() < 1e-9 && i == 0);
        // speed 2: halves the output timeline
        let (s, e, _) = map_interval_to_output(4.0, 6.0, &segs, 2.0).unwrap();
        assert!((s - 2.0).abs() < 1e-9 && (e - 3.0).abs() < 1e-9);
    }

    #[test]
    fn map_interval_multiple_segments_and_dropping() {
        // Keep 1–4 and 7–11: cumulative offsets 0 and 3.
        let segs = vec![Segment::keep(1.0, 4.0), Segment::keep(7.0, 11.0)];
        // A word in the first kept segment.
        let (s, e, i) = map_interval_to_output(2.0, 3.0, &segs, 1.0).unwrap();
        assert!((s - 1.0).abs() < 1e-9 && (e - 2.0).abs() < 1e-9 && i == 0);
        // A word in the second kept segment lands after the 3s of segment one.
        let (s, e, i) = map_interval_to_output(8.0, 9.0, &segs, 1.0).unwrap();
        assert!((s - 4.0).abs() < 1e-9 && (e - 5.0).abs() < 1e-9 && i == 1);
        // A word entirely inside the cut (4–7) is dropped.
        assert!(map_interval_to_output(5.0, 6.0, &segs, 1.0).is_none());
    }

    #[test]
    fn map_interval_straddling_a_cut_is_clamped() {
        let segs = vec![Segment::keep(0.0, 5.0), Segment::keep(10.0, 15.0)];
        // Word 4.5–6.0 straddles the cut; most overlap is in segment 0 (4.5–5.0).
        let (s, e, i) = map_interval_to_output(4.5, 6.0, &segs, 1.0).unwrap();
        assert_eq!(i, 0);
        assert!((s - 4.5).abs() < 1e-9);
        assert!((e - 5.0).abs() < 1e-9, "clamped to the kept boundary, got {e}");
    }

    #[test]
    fn output_resolution_accounts_for_crop() {
        let probe = probe_16x9(); // 1920x1080
        let plan = EditPlan { aspect: Some("9:16".into()), ..Default::default() };
        let (w, h) = output_resolution(&plan, &probe);
        assert_eq!(h, 1080);
        assert_eq!(w, 606); // 1080*9/16 = 607.5 → floor-even 606
        let plain = output_resolution(&EditPlan::default(), &probe);
        assert_eq!(plain, (1920, 1080));
    }

    #[test]
    fn group_lines_breaks_on_gap_and_word_count() {
        let t = transcript_fixture();
        let segs = vec![Segment::keep(0.0, 6.0)];
        let lines = group_caption_lines(&t, &segs, 1.0);
        // The 1.5→4.0 gap (2.5s > 0.8s) splits "one two three" from "four five six".
        assert_eq!(lines.len(), 2, "lines: {:?}", lines.iter().map(|l| l.text()).collect::<Vec<_>>());
        assert_eq!(lines[0].text(), "one two three");
        assert_eq!(lines[1].text(), "four five six");
        assert!((lines[0].start - 0.0).abs() < 1e-9);
        assert!((lines[1].start - 4.0).abs() < 1e-9);
    }

    #[test]
    fn group_lines_maps_output_times_under_speed_and_cuts() {
        let t = transcript_fixture();
        // Keep only 0–2 (drops four/five/six), at 2x speed → output halved.
        let plan = EditPlan { segments: vec![Segment::keep(0.0, 2.0)], speed: Some(2.0), ..Default::default() };
        let lines = group_caption_lines(&t, &effective_segments(&plan, &probe_16x9()), 2.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "one two three");
        assert!((lines[0].start - 0.0).abs() < 1e-9);
        // "three" ends at source 1.5 → output 0.75.
        assert!((lines[0].end - 0.75).abs() < 1e-9, "got {}", lines[0].end);
    }

    #[test]
    fn build_ass_shape_and_karaoke_tags() {
        let t = transcript_fixture();
        let probe = probe_16x9();
        let boxed = EditPlan { captions: true, caption_style: Some("boxed".into()), ..Default::default() };
        let doc = build_ass(&t, &boxed, &probe);
        assert!(doc.contains("PlayResX: 1920"));
        assert!(doc.contains("PlayResY: 1080"));
        assert!(doc.contains("BorderStyle")); // format header present
        assert!(doc.contains("BackColour,")); // header
        assert!(doc.contains(",3,")); // BorderStyle 3 (opaque box) for boxed
        assert!(doc.contains("Dialogue: 0,0:00:00.00,"));
        assert!(doc.contains("one two three"));

        let kara = EditPlan { captions: true, caption_style: Some("karaoke".into()), ..Default::default() };
        let doc = build_ass(&t, &kara, &probe);
        assert!(doc.contains("{\\k"), "karaoke should emit \\k tags:\n{doc}");
        // "one" onset 0.0 → next "two" onset 0.6 ⇒ 60 centiseconds.
        assert!(doc.contains("{\\k60}one"), "expected 60cs for 'one':\n{doc}");
    }

    #[test]
    fn build_srt_and_vtt_output() {
        let t = transcript_fixture();
        let probe = probe_16x9();
        let srt = build_subtitles(&t, "srt", None, &probe);
        assert!(srt.starts_with("1\n"));
        assert!(srt.contains("00:00:00,000 --> "));
        assert!(srt.contains("one two three"));
        let vtt = build_subtitles(&t, "vtt", None, &probe);
        assert!(vtt.starts_with("WEBVTT"));
        assert!(vtt.contains("00:00:00.000 --> "));
        // With a plan the times shift onto the output timeline (2x speed halves them).
        let plan = EditPlan { speed: Some(2.0), ..Default::default() };
        let srt2 = build_subtitles(&t, "srt", Some(&plan), &probe);
        assert!(srt2.contains("00:00:02,000 --> "), "four/five/six line at src 4s → out 2s:\n{srt2}");
    }

    #[test]
    fn validate_rejects_unknown_caption_style() {
        let probe = probe_16x9();
        let mut bad = EditPlan { captions: true, caption_style: Some("neon".into()), ..Default::default() };
        assert!(bad.validate(&probe).is_err());
        let mut ok = EditPlan { captions: true, caption_style: Some("karaoke".into()), ..Default::default() };
        assert!(ok.validate(&probe).is_ok());
    }

    fn filler_transcript() -> Transcript {
        // "so, um, I I think uh this is the the best take"
        Transcript {
            text: "so um I I think uh this is the the best take".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 6.0, text: "so um I I think uh this is the the best take".into() }],
            words: vec![
                TranscriptWord { start: 0.0, end: 0.2, word: "so".into() },
                TranscriptWord { start: 0.3, end: 0.6, word: "um,".into() }, // punctuation should still normalize
                TranscriptWord { start: 0.9, end: 1.0, word: "I".into() },
                TranscriptWord { start: 1.1, end: 1.2, word: "I".into() }, // stutter, 0.1s gap
                TranscriptWord { start: 1.3, end: 1.6, word: "think".into() },
                TranscriptWord { start: 1.7, end: 1.9, word: "Uh".into() }, // filler, case-insensitive
                TranscriptWord { start: 2.0, end: 2.2, word: "this".into() },
                TranscriptWord { start: 2.3, end: 2.4, word: "is".into() },
                TranscriptWord { start: 2.5, end: 2.6, word: "the".into() },
                TranscriptWord { start: 2.65, end: 2.75, word: "the".into() }, // stutter, 0.05s gap
                TranscriptWord { start: 2.9, end: 3.2, word: "best".into() },
                TranscriptWord { start: 3.3, end: 3.6, word: "take".into() },
            ],
        }
    }

    #[test]
    fn detect_filler_words_finds_um_and_uh_case_insensitively() {
        let t = filler_transcript();
        let cuts = detect_filler_words(&t);
        // "um," (0.3-0.6) and "Uh" (1.7-1.9) should both be cut despite case/punctuation.
        assert!(cuts.iter().any(|c| (c.start - 0.3).abs() < 1e-9 && (c.end - 0.6).abs() < 1e-9));
        assert!(cuts.iter().any(|c| (c.start - 1.7).abs() < 1e-9 && (c.end - 1.9).abs() < 1e-9));
    }

    #[test]
    fn detect_filler_words_cuts_first_occurrence_of_a_stutter() {
        let t = filler_transcript();
        let cuts = detect_filler_words(&t);
        // "I I" (0.9-1.0, 1.1-1.2, gap 0.1s < 0.5s): first "I" (0.9-1.0) is cut.
        assert!(cuts.iter().any(|c| (c.start - 0.9).abs() < 1e-9 && (c.end - 1.0).abs() < 1e-9));
    }

    #[test]
    fn detect_filler_words_skips_non_filler_words() {
        let t = filler_transcript();
        let cuts = detect_filler_words(&t);
        // "think", "this", "is", "best", "take" never trigger a cut on their own.
        for c in &cuts {
            assert!(c.end - c.start <= 0.35, "unexpected wide cut {:?} (not a single filler/stutter word)", c);
        }
    }

    #[test]
    fn detect_filler_words_merges_close_stutter_and_filler_cuts() {
        let t = filler_transcript();
        let cuts = detect_filler_words(&t);
        // The second stutter ("the the", 2.5-2.6 and 2.65-2.75, gap 0.05s < MERGE_GAP_S)
        // cuts the first "the" (2.5-2.6); nothing else sits within 0.15s of it, so it
        // should NOT get merged into an unrelated wider range.
        let the_cut = cuts.iter().find(|c| c.start > 2.4 && c.start < 2.6);
        assert!(the_cut.is_some(), "expected a cut near the second 'the the' stutter: {:?}", cuts);
    }

    #[test]
    fn detect_filler_words_empty_without_word_timings() {
        let t = Transcript {
            text: "um yeah".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 1.0, text: "um yeah".into() }],
            words: vec![],
        };
        assert!(detect_filler_words(&t).is_empty());
    }

    #[test]
    fn merge_ranges_combines_overlapping_and_close_ranges() {
        let ranges = vec![
            SilenceRange { start: 1.0, end: 2.0 },
            SilenceRange { start: 2.05, end: 3.0 }, // within 0.15s of the first -> merges
            SilenceRange { start: 10.0, end: 11.0 }, // far away -> stays separate
        ];
        let merged = merge_ranges(ranges, MERGE_GAP_S);
        assert_eq!(merged.len(), 2);
        assert!((merged[0].start - 1.0).abs() < 1e-9);
        assert!((merged[0].end - 3.0).abs() < 1e-9);
        assert!((merged[1].start - 10.0).abs() < 1e-9);
    }

    #[test]
    fn combined_keep_segments_merges_overlapping_silence_and_filler_cuts() {
        // A silence 5-6s and a filler word 5.9-6.05s overlap; combined they should cut
        // one continuous range (5-6.05), not leave a tiny sliver keep-segment.
        let silences = vec![SilenceRange { start: 5.0, end: 6.0 }];
        let fillers = vec![SilenceRange { start: 5.9, end: 6.05 }];
        let segs = combined_keep_segments(20.0, &silences, &fillers, 0.15);
        // Expect: 0-5.15 kept, then 5.85-20 kept (no sliver segment in between).
        assert_eq!(segs.len(), 2, "segments: {:?}", segs);
        assert!((segs[0].start - 0.0).abs() < 1e-9);
        assert!((segs[0].end - 5.15).abs() < 1e-6);
        assert!((segs[1].start - 5.9).abs() < 1e-6);
        assert!((segs[1].end - 20.0).abs() < 1e-9);
    }

    #[test]
    fn combined_keep_segments_drops_tiny_resulting_keeps() {
        // A filler cut sitting right at the start of the clip leaves only a hair's-width
        // "keep" segment before it once (a small) padding is applied — that sliver is
        // worthless and should be dropped rather than kept as a real segment. Note the
        // default 0.15s padding used elsewhere always yields >=0.15s boundary segments
        // (merge and padding share that constant), so this uses a smaller pad to exercise
        // the drop filter directly.
        let fillers = vec![SilenceRange { start: 0.05, end: 0.4 }];
        let segs = combined_keep_segments(10.0, &[], &fillers, 0.05);
        // Raw keep_segments would produce a {0, 0.1} sliver before the cut; it must be
        // filtered out, leaving only the long tail segment after the cut.
        assert!(
            !segs.iter().any(|s| s.start < 0.05),
            "expected the tiny leading sliver to be dropped: {:?}",
            segs
        );
        assert_eq!(segs.len(), 1, "segments: {:?}", segs);
        assert!((segs[0].start - 0.35).abs() < 1e-9);
        assert!((segs[0].end - 10.0).abs() < 1e-9);
    }

    #[test]
    fn combined_keep_segments_with_only_fillers_matches_filler_only_invert() {
        // No silences: combined should equal a plain filler-only inversion.
        let fillers = vec![SilenceRange { start: 3.0, end: 3.3 }];
        let combined = combined_keep_segments(15.0, &[], &fillers, 0.15);
        let plain = keep_segments(15.0, &fillers, 0.15);
        assert_eq!(combined.len(), plain.len());
        for (a, b) in combined.iter().zip(plain.iter()) {
            assert!((a.start - b.start).abs() < 1e-9);
            assert!((a.end - b.end).abs() < 1e-9);
        }
    }

    #[test]
    fn build_args_places_subtitles_after_crop() {
        let probe = probe_16x9();
        let plan = EditPlan {
            aspect: Some("9:16".into()),
            captions: true,
            caption_style: Some("boxed".into()),
            ..Default::default()
        };
        let ass = PathBuf::from("/tmp/muninn-video-x/captions.ass");
        let args = build_args_single(
            Path::new("/tmp/in.mp4"),
            &plan,
            &probe,
            "/tmp/f.ttc",
            &[],
            Some(&ass),
            VideoEncoder::Libx264,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("subtitles=filename=/tmp/muninn-video-x/captions.ass"));
        let crop_at = graph.find("crop=").expect("crop present");
        let subs_at = graph.find("subtitles=").expect("subtitles present");
        assert!(subs_at > crop_at, "subtitles must come after crop in the chain");
    }

    #[test]
    fn videotoolbox_bitrate_scales_with_resolution_and_clamps() {
        // 1920x1080@30fps: within the clamp range, scales with pixel count/fps.
        let hd = videotoolbox_bitrate_bps(1920, 1080, 30.0);
        assert!((2_000_000..=12_000_000).contains(&hd), "got {hd}");
        assert!(hd > 6_000_000 && hd < 9_000_000, "expected ~7.5Mbps for 1080p30, got {hd}");

        // Tiny output clamps to the 2Mbps floor.
        let tiny = videotoolbox_bitrate_bps(320, 180, 24.0);
        assert_eq!(tiny, 2_000_000);

        // Huge/high-fps output clamps to the 12Mbps ceiling.
        let huge = videotoolbox_bitrate_bps(3840, 2160, 60.0);
        assert_eq!(huge, 12_000_000);

        // A missing/zero fps falls back to a 30fps assumption rather than zeroing out.
        let no_fps = videotoolbox_bitrate_bps(1920, 1080, 0.0);
        assert_eq!(no_fps, hd);
    }

    #[test]
    fn pick_encoder_is_software_when_not_preferred() {
        // Deterministic regardless of host: prefer_hw=false must never pick hardware.
        assert_eq!(pick_encoder(false), VideoEncoder::Libx264);
    }

    #[test]
    fn build_args_videotoolbox_uses_bitrate_not_crf() {
        let probe = probe_16x9();
        let plan = EditPlan::default();
        let args = build_args_single(
            Path::new("/tmp/in.mp4"),
            &plan,
            &probe,
            "/tmp/f.ttc",
            &[],
            None,
            VideoEncoder::H264Videotoolbox,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();
        assert!(args.iter().any(|a| a == "h264_videotoolbox"));
        assert!(args.iter().any(|a| a == "-b:v"));
        assert!(!args.iter().any(|a| a == "-crf"), "videotoolbox has no CRF mode, args: {args:?}");
        assert!(!args.iter().any(|a| a == "libx264"));
        let bv_idx = args.iter().position(|a| a == "-b:v").unwrap();
        let expected = videotoolbox_bitrate_bps(probe.width, probe.height, probe.fps).to_string();
        assert_eq!(args[bv_idx + 1], expected);
        // pix_fmt/movflags stay the same across encoders.
        assert!(args.iter().any(|a| a == "yuv420p"));
        assert!(args.iter().any(|a| a == "+faststart"));
    }

    #[test]
    fn build_args_multi_source_inputs_and_normalization() {
        let main = probe_16x9(); // 1920x1080, 30fps, audio
        let broll = VideoProbe {
            path: "/tmp/broll.mp4".into(),
            duration_s: 30.0,
            width: 1280,
            height: 720,
            fps: 25.0,
            has_audio: true,
            video_codec: "h264".into(),
            size_bytes: 1,
        };
        let inputs = vec![
            SourceInput { path: PathBuf::from("/tmp/main.mp4"), probe: main, is_image: false },
            SourceInput { path: PathBuf::from("/tmp/broll.mp4"), probe: broll, is_image: false },
            SourceInput { path: PathBuf::from("/tmp/card.png"), probe: image_probe("/tmp/card.png", 800, 600), is_image: true },
        ];
        let plan = EditPlan {
            segments: vec![
                Segment::keep(0.0, 5.0),
                Segment { start: 2.0, end: 4.0, source: Some("/tmp/broll.mp4".into()) },
                Segment { start: 0.0, end: 3.0, source: Some("/tmp/card.png".into()) },
            ],
            ..Default::default()
        };
        let args = build_render_args(
            &inputs,
            &plan,
            "/tmp/f.ttc",
            &[],
            None,
            None,
            VideoEncoder::Libx264,
            Path::new("/tmp/out.mp4"),
        )
        .unwrap();

        // Three distinct `-i` inputs, in index order.
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 3);
        let i0 = args.iter().position(|a| a == "/tmp/main.mp4").unwrap();
        let i1 = args.iter().position(|a| a == "/tmp/broll.mp4").unwrap();
        let i2 = args.iter().position(|a| a == "/tmp/card.png").unwrap();
        assert!(i0 < i1 && i1 < i2, "inputs must stay in index order");
        // Image input is looped for exactly the longest segment that uses it (3s here).
        assert!(args.iter().any(|a| a == "-loop"));
        assert!(args.windows(2).any(|w| w[0] == "-t" && w[1] == "3.0000"), "image -t: {args:?}");

        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("concat=n=3:v=1:a=1"), "graph: {graph}");
        // Main segment is untouched (no scale/crop normalization on [0:v]).
        assert!(graph.contains("[0:v]trim=start=0.0000:end=5.0000,setpts=PTS-STARTPTS[v0];"));
        // Both inserts are normalized to the MAIN dims/fps/sar and yuv420p before concat.
        assert!(graph.contains("[1:v]trim=start=2.0000:end=4.0000"), "broll video: {graph}");
        assert!(graph.contains("[2:v]trim=start=0.0000:end=3.0000"), "image video: {graph}");
        assert_eq!(
            graph.matches("scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,setsar=1,fps=30.0000,format=yuv420p").count(),
            2,
            "both inserts normalized: {graph}"
        );
        // Real-audio branches (main + b-roll) get a canonical aformat once inserts are present.
        assert!(graph.contains("[0:a]atrim=start=0.0000:end=5.0000"));
        assert!(graph.contains("[1:a]atrim=start=2.0000:end=4.0000"));
        assert_eq!(
            graph.matches("aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo").count(),
            3, // main audio + b-roll audio + the image's silent branch
            "graph: {graph}"
        );
        // The silent image insert synthesizes matching silence, no [2:a] input pad.
        assert!(graph.contains("anullsrc=channel_layout=stereo:sample_rate=48000,atrim=end=3.0000"));
        assert!(!graph.contains("[2:a]"), "image has no audio stream to map: {graph}");
    }

    #[test]
    fn build_args_single_source_unchanged_no_aformat() {
        // With only the main input, the graph must stay exactly as before — no aformat, no
        // anullsrc — so single-source renders are byte-for-byte unaffected.
        let probe = probe_16x9();
        let plan = EditPlan {
            segments: vec![Segment::keep(0.0, 5.0), Segment::keep(10.0, 15.0)],
            ..Default::default()
        };
        let args = build_args_single(
            Path::new("/tmp/in.mp4"), &plan, &probe, "/tmp/f.ttc", &[], None, VideoEncoder::Libx264, Path::new("/tmp/out.mp4"),
        ).unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(!graph.contains("aformat="), "single source needs no aformat: {graph}");
        assert!(!graph.contains("anullsrc"), "single source needs no anullsrc: {graph}");
        assert!(!graph.contains("force_original_aspect_ratio"), "no insert normalization: {graph}");
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);
    }

    #[test]
    fn build_args_image_insert_is_silent_when_main_muted_stays_valid() {
        // mute=true with an image insert: no audio chain at all, but the image still renders.
        let main = probe_16x9();
        let inputs = vec![
            SourceInput { path: PathBuf::from("/tmp/main.mp4"), probe: main, is_image: false },
            SourceInput { path: PathBuf::from("/tmp/card.png"), probe: image_probe("/tmp/card.png", 400, 400), is_image: true },
        ];
        let plan = EditPlan {
            mute: true,
            segments: vec![
                Segment { start: 0.0, end: 3.0, source: Some("/tmp/card.png".into()) },
                Segment::keep(0.0, 5.0),
            ],
            ..Default::default()
        };
        let args = build_render_args(&inputs, &plan, "/tmp/f.ttc", &[], None, None, VideoEncoder::Libx264, Path::new("/tmp/out.mp4")).unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("concat=n=2:v=1:a=0"), "graph: {graph}");
        assert!(!graph.contains("anullsrc"), "muted → no audio work at all: {graph}");
        assert!(!graph.contains("[aout]"));
        assert!(args.iter().any(|a| a == "-an"));
    }

    #[test]
    fn validate_image_insert_duration_bounds() {
        let probe = probe_16x9(); // 60s main
        let mut too_short = EditPlan {
            segments: vec![Segment { start: 0.0, end: 0.1, source: Some("/x/card.png".into()) }],
            ..Default::default()
        };
        assert!(too_short.validate(&probe).is_err());
        let mut too_long = EditPlan {
            segments: vec![Segment { start: 0.0, end: 40.0, source: Some("card.png".into()) }],
            ..Default::default()
        };
        assert!(too_long.validate(&probe).is_err());
        let mut empty_src = EditPlan {
            segments: vec![Segment { start: 0.0, end: 3.0, source: Some("   ".into()) }],
            ..Default::default()
        };
        assert!(empty_src.validate(&probe).is_err());
        // A b-roll VIDEO insert whose end exceeds the MAIN duration must NOT be clamped or
        // rejected — its range is on the insert's own timeline, checked at render time.
        let mut broll = EditPlan {
            segments: vec![Segment { start: 0.0, end: 500.0, source: Some("/x/b.mp4".into()) }],
            ..Default::default()
        };
        assert!(broll.validate(&probe).is_ok());
        let mut ok = EditPlan {
            segments: vec![Segment { start: 0.0, end: 3.0, source: Some("/x/card.png".into()) }],
            ..Default::default()
        };
        assert!(ok.validate(&probe).is_ok());
    }

    #[test]
    fn map_interval_skips_inserts_but_offset_shifts_after_them() {
        // main A: source 0..5, insert B: a 3s image card, main C: source 5..10.
        let segs = vec![
            Segment::keep(0.0, 5.0),
            Segment { start: 0.0, end: 3.0, source: Some("/tmp/card.png".into()) },
            Segment::keep(5.0, 10.0),
        ];
        // A word at source 7 (inside C) lands 3s later on the output timeline because of the
        // inserted card: output offset for C = 5 (A) + 3 (insert) = 8, plus (7-5) = 10.
        let (s, e, i) = map_interval_to_output(7.0, 7.5, &segs, 1.0).unwrap();
        assert_eq!(i, 2);
        assert!((s - 10.0).abs() < 1e-9, "expected 10.0, got {s}");
        assert!((e - 10.5).abs() < 1e-9, "got {e}");
        // A word at source 1.5 overlaps A AND the insert's 0..3 slot — it must map to A (no leak).
        let (s, _e, i) = map_interval_to_output(1.5, 2.0, &segs, 1.0).unwrap();
        assert_eq!(i, 0, "word must not leak into the insert slot");
        assert!((s - 1.5).abs() < 1e-9);
        // Under 2x speed the after-insert word halves on the output timeline: 10 → 5.
        let (s, _e, _i) = map_interval_to_output(7.0, 7.5, &segs, 2.0).unwrap();
        assert!((s - 5.0).abs() < 1e-9, "expected 5.0 at 2x, got {s}");
    }

    /// Full pipeline against the real hardware encoder — needs ffmpeg built with
    /// h264_videotoolbox support (skips itself if not present), so it's ignored in CI.
    /// Run locally with: cargo test --lib video -- --ignored
    #[test]
    #[ignore]
    fn render_args_run_end_to_end_with_hardware_encoder() {
        let encoder = pick_encoder(true);
        if encoder != VideoEncoder::H264Videotoolbox {
            eprintln!("h264_videotoolbox not available on this ffmpeg build — skipping");
            return;
        }

        let dir = std::env::temp_dir().join("muninn-video-hw-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let status = Command::new(ffmpeg_path().unwrap())
            .args([
                "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=5",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
                "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());

        let probe = probe_video(&src).unwrap();
        let plan = EditPlan::default();
        let out = dir.join("out-hw.mp4");
        let args =
            build_args_single(&src, &plan, &probe, font_path().unwrap(), &[], None, encoder, &out).unwrap();
        assert!(args.iter().any(|a| a == "h264_videotoolbox"));

        let result = Command::new(ffmpeg_path().unwrap()).args(&args).output().unwrap();
        assert!(
            result.status.success(),
            "hardware-encoded ffmpeg render failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let out_probe = probe_video(&out).unwrap();
        assert!((out_probe.duration_s - probe.duration_s).abs() < 0.5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- smart reframe tests ----------

    /// Build synthetic detection frames at `fps`: for each (start,end,cx) span, a face of
    /// width `fw` centered at normalized x=`cx`, confidence 0.95.
    fn frames_from_spans(spans: &[(f64, f64, f64)], fw: f64, duration: f64, fps: f64) -> Vec<FrameFaces> {
        let step = 1.0 / fps;
        let mut out = Vec::new();
        let mut t = 0.0;
        while t < duration - 1e-9 {
            let cx = spans.iter().find(|(s, e, _)| t >= *s && t < *e).map(|(_, _, c)| *c);
            let faces = match cx {
                Some(c) => vec![Face { x: (c - fw / 2.0).max(0.0), y: 0.3, w: fw, h: 0.25, c: 0.95 }],
                None => vec![],
            };
            out.push(FrameFaces { t, faces });
            t += step;
        }
        out
    }

    #[test]
    fn subject_center_is_confidence_and_area_weighted() {
        // A big confident face at 0.7 should dominate a tiny low-confidence one at 0.2.
        let fr = FrameFaces {
            t: 0.0,
            faces: vec![
                Face { x: 0.65, y: 0.3, w: 0.10, h: 0.20, c: 0.99 }, // center 0.70, area 0.02
                Face { x: 0.18, y: 0.3, w: 0.04, h: 0.05, c: 0.30 }, // center 0.20, tiny
            ],
        };
        let centers = subject_centers(&[fr]);
        let cx = centers[0].1.unwrap();
        assert!(cx > 0.66 && cx < 0.71, "large confident face should dominate, got {cx}");
        // A face-less frame yields None (caller holds the previous center).
        assert_eq!(subject_centers(&[FrameFaces { t: 1.0, faces: vec![] }])[0].1, None);
    }

    #[test]
    fn hysteresis_ignores_small_wobble() {
        // Window ~0.32 wide → band ±0.064. A ±0.03 wobble around 0.5 must NOT move the crop.
        let win_norm = 0.32;
        let centers = vec![
            (0.0, Some(0.50)),
            (0.5, Some(0.53)),
            (1.0, Some(0.47)),
            (1.5, Some(0.52)),
            (2.0, Some(0.49)),
        ];
        let wins = hysteresis_windows(&centers, win_norm, 3.0);
        assert_eq!(wins.len(), 1, "small wobble should stay one window: {wins:?}");
        assert!((wins[0].cx - 0.50).abs() < 1e-9);
        assert!((wins[0].end - 3.0).abs() < 1e-9);
    }

    #[test]
    fn hysteresis_one_clean_jump_on_sustained_move() {
        // Subject at 0.3 for 5s then 0.7 for 5s — one jump, committed after the 0.5s dwell.
        let win_norm = 0.32;
        let frames = frames_from_spans(&[(0.0, 5.0, 0.3), (5.0, 10.0, 0.7)], 0.12, 10.0, 2.0);
        let centers = subject_centers(&frames);
        let wins = hysteresis_windows(&centers, win_norm, 10.0);
        assert_eq!(wins.len(), 2, "expected exactly one reframe jump: {wins:?}");
        assert!((wins[0].cx - 0.3).abs() < 1e-6);
        assert!((wins[1].cx - 0.7).abs() < 1e-6);
        // The boundary sits at the moment the subject left the band (~5.0s), not 0.5s later.
        assert!((wins[0].end - 5.0).abs() < 0.6, "jump near 5.0s, got {}", wins[0].end);
    }

    #[test]
    fn hysteresis_no_faces_yields_no_windows() {
        let centers = vec![(0.0, None), (0.5, None), (1.0, None)];
        assert!(hysteresis_windows(&centers, 0.32, 1.5).is_empty());
    }

    #[test]
    fn build_output_pieces_centers_insert_slots() {
        // main[0,5], image insert (2s), main[5,10]; one steady subject window at 0.3.
        let src_wins = vec![SrcWin { start: 0.0, end: 10.0, cx: 0.3 }];
        let segs = vec![
            Segment::keep(0.0, 5.0),
            Segment { start: 0.0, end: 2.0, source: Some("/x/card.png".into()) },
            Segment::keep(5.0, 10.0),
        ];
        let in_w = 1920.0;
        let cw = 606u32;
        let max_x = 1920.0 - cw as f64; // 1314
        let center_x = (max_x / 2.0).round(); // 657
        let subject_x = crop_x_for(0.3, in_w, cw, max_x); // 0.3*1920-303 = 273
        let pieces = build_output_pieces(&src_wins, &segs, 1.0, in_w, cw, max_x, center_x);
        // subject(273) for the first main slot, centered(657) across the insert [5,7), then
        // back to subject(273) for the last main slot.
        assert_eq!(pieces.len(), 3, "pieces: {pieces:?}");
        assert!((pieces[0].out_start - 0.0).abs() < 1e-9 && (pieces[0].x - subject_x).abs() < 1e-9);
        assert!((pieces[1].out_start - 5.0).abs() < 1e-9 && (pieces[1].x - center_x).abs() < 1e-9);
        assert!((pieces[2].out_start - 7.0).abs() < 1e-9 && (pieces[2].x - subject_x).abs() < 1e-9);
    }

    #[test]
    fn build_output_pieces_maps_through_cuts_and_speed() {
        // Cut removes source 5..20; speed 2x. A subject window changes across the cut.
        let src_wins = vec![SrcWin { start: 0.0, end: 10.0, cx: 0.3 }, SrcWin { start: 10.0, end: 25.0, cx: 0.7 }];
        let segs = vec![Segment::keep(0.0, 5.0), Segment::keep(20.0, 25.0)];
        let in_w = 1920.0;
        let cw = 606u32;
        let max_x = 1920.0 - cw as f64;
        let center_x = (max_x / 2.0).round();
        let pieces = build_output_pieces(&src_wins, &segs, 2.0, in_w, cw, max_x, center_x);
        // seg0 (src 0..5) → output 0..2.5 at x for cx 0.3; seg1 (src 20..25) starts at output
        // 2.5 (5s of source / 2x) at x for cx 0.7.
        assert_eq!(pieces.len(), 2, "pieces: {pieces:?}");
        assert!((pieces[0].out_start - 0.0).abs() < 1e-9);
        assert!((pieces[0].x - crop_x_for(0.3, in_w, cw, max_x)).abs() < 1e-9);
        assert!((pieces[1].out_start - 2.5).abs() < 1e-9, "output-time boundary, got {}", pieces[1].out_start);
        assert!((pieces[1].x - crop_x_for(0.7, in_w, cw, max_x)).abs() < 1e-9);
    }

    #[test]
    fn post_process_merges_short_windows() {
        // A 0.4s sliver between two long windows must be absorbed into its closest-x neighbor.
        let pieces = vec![
            OutPiece { out_start: 0.0, x: 200.0 },
            OutPiece { out_start: 4.0, x: 210.0 }, // 0.4s sliver, closer to the 200 window
            OutPiece { out_start: 4.4, x: 900.0 },
        ];
        let out = post_process_pieces(pieces, 10.0, 1.0, 60, 1314.0);
        assert_eq!(out.len(), 2, "sliver should merge away: {out:?}");
        assert!((out[0].x - 200.0).abs() < 1e-9);
        assert!((out[1].x - 900.0).abs() < 1e-9);
        // The sliver was absorbed into the previous (200) window, which now runs up to where
        // the sliver ended (4.4s) — that's where the 900 window begins.
        assert!((out[1].out_start - 4.4).abs() < 1e-9, "got {}", out[1].out_start);
    }

    #[test]
    fn post_process_caps_window_count() {
        let mut pieces = Vec::new();
        for i in 0..100 {
            pieces.push(OutPiece { out_start: i as f64 * 2.0, x: (i % 2) as f64 * 500.0 });
        }
        let out = post_process_pieces(pieces, 200.0, 1.0, 60, 1314.0);
        assert!(out.len() <= 60, "must cap at 60, got {}", out.len());
    }

    #[test]
    fn post_process_clamps_x() {
        let pieces = vec![OutPiece { out_start: 0.0, x: -50.0 }, OutPiece { out_start: 5.0, x: 99999.0 }];
        let out = post_process_pieces(pieces, 10.0, 1.0, 60, 1314.0);
        assert!(out.iter().all(|p| p.x >= 0.0 && p.x <= 1314.0), "clamped: {out:?}");
        assert!((out[0].x - 0.0).abs() < 1e-9);
        assert!((out[1].x - 1314.0).abs() < 1e-9);
    }

    #[test]
    fn pieces_to_expr_single_and_nested() {
        assert_eq!(pieces_to_expr(&[OutPiece { out_start: 0.0, x: 657.0 }]), "657");
        let expr = pieces_to_expr(&[
            OutPiece { out_start: 0.0, x: 273.0 },
            OutPiece { out_start: 5.0, x: 1041.0 },
        ]);
        assert_eq!(expr, "if(lt(t\\,5.000)\\,273\\,1041)");
    }

    #[test]
    fn compute_reframe_track_no_faces_is_none() {
        let probe = probe_16x9();
        let plan = EditPlan { aspect: Some("9:16".into()), smart_reframe: true, ..Default::default() };
        let frames = frames_from_spans(&[], 0.12, 10.0, 2.0); // all face-less
        assert!(compute_reframe_track(&frames, &probe, &plan).is_none());
    }

    #[test]
    fn compute_reframe_track_non_narrowing_aspect_is_none() {
        let probe = probe_16x9(); // 1920x1080
        // 16:9 target on a 16:9 source does not narrow horizontally → no reframe.
        let plan = EditPlan { aspect: Some("16:9".into()), smart_reframe: true, ..Default::default() };
        let frames = frames_from_spans(&[(0.0, 10.0, 0.5)], 0.12, 10.0, 2.0);
        assert!(compute_reframe_track(&frames, &probe, &plan).is_none());
    }

    #[test]
    fn compute_reframe_track_steady_subject_static_expr() {
        let probe = probe_16x9();
        let plan = EditPlan { aspect: Some("9:16".into()), smart_reframe: true, ..Default::default() };
        // Subject steadily off-center at 0.3 → one window, no if(), x = 0.3*1920 - 303 = 273.
        let frames = frames_from_spans(&[(0.0, 10.0, 0.3)], 0.12, 10.0, 2.0);
        let rf = compute_reframe_track(&frames, &probe, &plan).unwrap();
        assert_eq!(rf.crop_w, 606);
        assert_eq!(rf.crop_h, 1080);
        assert_eq!(rf.crop_y, 0); // full-height crop → no vertical offset
        assert!(!rf.x_expr.contains("if("), "steady subject → static x: {}", rf.x_expr);
        assert_eq!(rf.x_expr, "273");
    }

    #[test]
    fn compute_reframe_track_moving_subject_has_one_jump_and_clamps() {
        let probe = probe_16x9();
        let plan = EditPlan { aspect: Some("9:16".into()), smart_reframe: true, ..Default::default() };
        // Subject far-left (0.05) then far-right (0.95): both crop-x values clamp to the frame.
        let frames = frames_from_spans(&[(0.0, 5.0, 0.05), (5.0, 10.0, 0.95)], 0.12, 10.0, 2.0);
        let rf = compute_reframe_track(&frames, &probe, &plan).unwrap();
        assert!(rf.x_expr.contains("if(lt(t"), "expected one jump: {}", rf.x_expr);
        // 0.05*1920-303 = -207 → clamps to 0; 0.95*1920-303 = 1521 → clamps to 1314 (in_w-cw).
        assert!(rf.x_expr.contains("\\,0\\,"), "left edge clamps to 0: {}", rf.x_expr);
        assert!(rf.x_expr.ends_with("\\,1314)"), "right edge clamps to 1314: {}", rf.x_expr);
    }

    /// End-to-end smart-reframe render against real ffmpeg. testsrc2 has no faces (and the
    /// framescan sidecar may not be built at all), so the reframe MUST fall back to a static
    /// center crop and the render MUST still succeed. Ignored in CI. Run with:
    /// cargo test --lib video -- --ignored
    #[test]
    #[ignore]
    fn smart_reframe_falls_back_to_center_crop_e2e() {
        let dir = std::env::temp_dir().join("muninn-reframe-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let status = Command::new(ffmpeg_path().unwrap())
            .args([
                "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=6",
                "-c:v", "libx264", "-preset", "ultrafast",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());

        let probe = probe_video(&src).unwrap();
        let plan = EditPlan { aspect: Some("9:16".into()), smart_reframe: true, ..Default::default() };

        // The scan finds no faces (or the sidecar is missing) → None → static center crop.
        let reframe = compute_reframe_for_render(&src, &probe, &plan);
        assert!(reframe.is_none(), "testsrc2 has no faces — reframe must fall back");

        let out = dir.join("out.mp4");
        let args = build_args_single(
            &src, &plan, &probe, font_path().unwrap(), &[], None, VideoEncoder::Libx264, &out,
        )
        .unwrap();
        let result = Command::new(ffmpeg_path().unwrap()).args(&args).output().unwrap();
        assert!(result.status.success(), "fallback render failed:\n{}", String::from_utf8_lossy(&result.stderr));
        let out_probe = probe_video(&out).unwrap();
        let ratio = out_probe.width as f64 / out_probe.height as f64;
        assert!((ratio - 9.0 / 16.0).abs() < 0.02, "expected ~9:16, got {}x{}", out_probe.width, out_probe.height);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- auto-shorts: spec validation, slug naming, cancel flag -----

    fn spec(title: &str, ranges: &[(f64, f64)]) -> ShortSpec {
        ShortSpec {
            title: title.into(),
            segments: ranges.iter().map(|&(s, e)| Segment::keep(s, e)).collect(),
            hook: "hook".into(),
            rationale: "why".into(),
        }
    }

    /// 600s source so target=45 (window 22.5–90s) is comfortably inside.
    fn probe_long() -> VideoProbe {
        VideoProbe { duration_s: 600.0, ..probe_16x9() }
    }

    #[test]
    fn sanitize_shorts_selects_best_count_in_order() {
        let probe = probe_long();
        let specs = vec![
            spec("a", &[(0.0, 45.0)]),
            spec("b", &[(60.0, 105.0)]),
            spec("c", &[(120.0, 165.0)]),
            spec("d", &[(200.0, 245.0)]),
        ];
        let out = sanitize_short_specs(specs, &probe, 2, 45.0).unwrap();
        assert_eq!(out.len(), 2, "take best `count`");
        assert_eq!(out[0].title, "a");
        assert_eq!(out[1].title, "b");
    }

    #[test]
    fn sanitize_shorts_rejects_too_many_specs() {
        let probe = probe_long();
        // count=1 ⇒ at most 2 specs allowed; 3 is garbage.
        let specs = vec![spec("a", &[(0.0, 45.0)]), spec("b", &[(60.0, 105.0)]), spec("c", &[(120.0, 165.0)])];
        assert!(sanitize_short_specs(specs, &probe, 1, 45.0).is_err());
    }

    #[test]
    fn sanitize_shorts_errors_on_empty() {
        assert!(sanitize_short_specs(vec![], &probe_long(), 3, 45.0).is_err());
    }

    #[test]
    fn sanitize_shorts_clamps_segments_to_source() {
        let probe = probe_long();
        // start below 0 and end past the 600s source are clamped in.
        let out = sanitize_short_specs(vec![spec("x", &[(-10.0, 45.0)])], &probe, 3, 45.0).unwrap();
        assert_eq!(out.len(), 1);
        assert!((out[0].segments[0].start - 0.0).abs() < 1e-9);
        assert!((out[0].segments[0].end - 45.0).abs() < 1e-9);

        let clamped_end = sanitize_short_specs(vec![spec("x", &[(560.0, 999.0)])], &probe, 3, 45.0).unwrap();
        assert!((clamped_end[0].segments[0].end - 600.0).abs() < 1e-9, "end clamps to source duration");
    }

    #[test]
    fn sanitize_shorts_drops_out_of_range_durations() {
        let probe = probe_long();
        // target 45 → allowed 22.5–90s. 10s is too short, 120s too long; both dropped.
        let specs = vec![spec("short", &[(0.0, 10.0)]), spec("long", &[(0.0, 120.0)]), spec("ok", &[(0.0, 45.0)])];
        let out = sanitize_short_specs(specs, &probe, 3, 45.0).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "ok");
    }

    #[test]
    fn sanitize_shorts_rejects_overlapping_segments_within_a_spec() {
        let probe = probe_long();
        // Two overlapping ranges (10–40 and 30–60) in one spec ⇒ that spec is dropped.
        let bad = spec("overlap", &[(10.0, 40.0), (30.0, 60.0)]);
        let good = spec("stitched", &[(0.0, 20.0), (100.0, 130.0)]); // total 50s, non-overlapping
        let out = sanitize_short_specs(vec![bad, good], &probe, 3, 45.0).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "stitched");
        // Multi-segment kept and sorted.
        assert_eq!(out[0].segments.len(), 2);
        assert!(out[0].segments[0].start <= out[0].segments[1].start);
    }

    #[test]
    fn sanitize_shorts_sorts_segments_and_strips_insert_source() {
        let probe = probe_long();
        // Out-of-order, and one segment carries an insert `source` that must be stripped (shorts
        // are cut from the main video only).
        let mut s = spec("s", &[(100.0, 130.0), (0.0, 20.0)]);
        s.segments[0].source = Some("/tmp/broll.mp4".into());
        let out = sanitize_short_specs(vec![s], &probe, 3, 45.0).unwrap();
        assert_eq!(out.len(), 1);
        assert!((out[0].segments[0].start - 0.0).abs() < 1e-9, "sorted ascending");
        assert!(out[0].segments.iter().all(|seg| seg.source.is_none()), "insert sources stripped");
    }

    #[test]
    fn sanitize_shorts_truncates_title_to_60_chars() {
        let probe = probe_long();
        let long_title = "z".repeat(200);
        let out = sanitize_short_specs(vec![spec(&long_title, &[(0.0, 45.0)])], &probe, 3, 45.0).unwrap();
        assert_eq!(out[0].title.chars().count(), 60);

        let blank = sanitize_short_specs(vec![spec("   ", &[(0.0, 45.0)])], &probe, 3, 45.0).unwrap();
        assert_eq!(blank[0].title, "Untitled clip");
    }

    #[test]
    fn sanitize_stem_slugifies_titles_for_output_names() {
        assert_eq!(sanitize_stem("The Pricing Discussion!"), "The-Pricing-Discussion");
        assert_eq!(sanitize_stem("  spaces  &&  symbols  "), "spaces-symbols");
        assert_eq!(sanitize_stem("already-kebab_case"), "already-kebab_case");
        assert_eq!(sanitize_stem("!!!"), "edit"); // nothing usable → fallback stem
    }

    #[test]
    fn shorts_cancel_flag_roundtrips() {
        let job = "test-shorts-cancel-job";
        clear_pipeline_cancel(job);
        assert!(!is_pipeline_cancelled(job));
        // request_cancel_shorts sets the flag (and would kill any active sub-render — none here).
        request_cancel_shorts(job);
        assert!(is_pipeline_cancelled(job));
        clear_pipeline_cancel(job);
        assert!(!is_pipeline_cancelled(job), "flag cleared for reuse");
    }

    #[test]
    fn compile_cancel_flag_roundtrips() {
        let job = "test-compile-cancel-job";
        clear_pipeline_cancel(job);
        assert!(!is_pipeline_cancelled(job));
        // request_cancel_compile flags the job (and would kill the active render — none here).
        request_cancel_compile(job);
        assert!(is_pipeline_cancelled(job));
        clear_pipeline_cancel(job);
        assert!(!is_pipeline_cancelled(job), "flag cleared for reuse");
    }

    // ---- cross-source captions ----

    /// Output order: MAIN[src 0..3] then clipB[src 0..2]. Pre-speed offsets: main→0, clipB→3.
    fn two_source_segments() -> Vec<Segment> {
        vec![
            Segment::keep(0.0, 3.0),
            Segment { start: 0.0, end: 2.0, source: Some("clipB.mp4".into()) },
        ]
    }

    #[test]
    fn map_interval_source_aware_offsets() {
        let segs = two_source_segments();
        // A MAIN word (source None) at 1.0–1.5 → output 1.0–1.5, in segment 0.
        let (s, e, i) = map_interval_to_output_for_source(1.0, 1.5, &None, &segs, 1.0).unwrap();
        assert_eq!(i, 0);
        assert!((s - 1.0).abs() < 1e-9 && (e - 1.5).abs() < 1e-9);
        // A clipB word at clipB-time 0.5–1.0 → output 3.5–4.0 (offset 3 + 0.5..1.0), in segment 1.
        let bsrc = Some("clipB.mp4".to_string());
        let (s, e, i) = map_interval_to_output_for_source(0.5, 1.0, &bsrc, &segs, 1.0).unwrap();
        assert_eq!(i, 1);
        assert!((s - 3.5).abs() < 1e-9, "clipB start after main's 3s, got {s}");
        assert!((e - 4.0).abs() < 1e-9, "got {e}");
    }

    #[test]
    fn map_interval_source_no_leak() {
        let segs = two_source_segments();
        let bsrc = Some("clipB.mp4".to_string());
        // A clipB word at the SAME source time as a valid main word (2.5) does NOT get captured by
        // main's segment — only clipB's segments are considered, and 2.5 is outside clipB's 0..2 →
        // dropped entirely rather than leaking into the main slot.
        assert!(map_interval_to_output_for_source(2.5, 2.8, &bsrc, &segs, 1.0).is_none());
        // The same source time for a MAIN word maps into the main segment (output 2.5).
        let (s, _e, i) = map_interval_to_output_for_source(2.5, 2.8, &None, &segs, 1.0).unwrap();
        assert_eq!(i, 0);
        assert!((s - 2.5).abs() < 1e-9);
    }

    #[test]
    fn build_ass_multi_places_each_source_in_its_slot() {
        let probe = probe_16x9();
        let plan = EditPlan { segments: two_source_segments(), captions: true, ..Default::default() };
        let mut tmap = TranscriptMap::default();
        tmap.insert(None, Transcript {
            text: "alpha beta".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 3.0, text: "alpha beta".into() }],
            words: vec![
                TranscriptWord { start: 0.5, end: 1.0, word: "alpha".into() },
                TranscriptWord { start: 1.0, end: 1.5, word: "beta".into() },
            ],
        });
        tmap.insert(Some("clipB.mp4"), Transcript {
            text: "gamma delta".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 2.0, text: "gamma delta".into() }],
            words: vec![
                TranscriptWord { start: 0.2, end: 0.6, word: "gamma".into() },
                TranscriptWord { start: 0.6, end: 1.0, word: "delta".into() },
            ],
        });
        let doc = build_ass_multi(&tmap, &plan, &probe);
        assert!(doc.contains("alpha beta"), "main line:\n{doc}");
        assert!(doc.contains("gamma delta"), "clipB line:\n{doc}");
        // Main line starts at output 0.5; clipB line at output 3.2 (offset 3 + 0.2).
        assert!(doc.contains("Dialogue: 0,0:00:00.50,"), "main line at 0.5:\n{doc}");
        assert!(doc.contains("Dialogue: 0,0:00:03.20,"), "clipB line at 3.2:\n{doc}");
    }

    #[test]
    fn build_subtitles_multi_with_speed() {
        let probe = probe_16x9();
        // 2x speed halves output times.
        let plan = EditPlan { segments: two_source_segments(), speed: Some(2.0), ..Default::default() };
        let mut tmap = TranscriptMap::default();
        tmap.insert(None, Transcript {
            text: "hi".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 3.0, text: "hi".into() }],
            words: vec![TranscriptWord { start: 0.5, end: 1.0, word: "hi".into() }],
        });
        tmap.insert(Some("clipB.mp4"), Transcript {
            text: "yo".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 2.0, text: "yo".into() }],
            words: vec![TranscriptWord { start: 0.2, end: 0.6, word: "yo".into() }],
        });
        let srt = build_subtitles_multi(&tmap, "srt", Some(&plan), &probe);
        // clipB word at 0.2 → (offset 3 + 0.2)/2 = 1.6s.
        assert!(srt.contains("00:00:01,600 --> "), "clipB at out 1.6s under 2x:\n{srt}");
        // main word at 0.5 → 0.25s.
        assert!(srt.contains("00:00:00,250 --> "), "main at out 0.25s under 2x:\n{srt}");
    }

    #[test]
    fn source_without_transcript_is_caption_less() {
        let probe = probe_16x9();
        let plan = EditPlan { segments: two_source_segments(), captions: true, ..Default::default() };
        let mut tmap = TranscriptMap::default();
        // Only MAIN has a transcript; clipB has none → clipB contributes no caption lines.
        tmap.insert(None, transcript_fixture());
        let doc = build_ass_multi(&tmap, &plan, &probe);
        // Main words inside the kept 0..3 segment are captioned…
        assert!(doc.contains("one two three"), "kept main words:\n{doc}");
        // …but words outside it (four/five/six at src 4..5.5) are dropped, and there is nothing
        // in clipB's slot (no transcript). No dialogue line starts at/after output 3.0s.
        assert!(!doc.contains("four five six"), "words outside the kept main segment are dropped");
        assert!(!doc.contains("Dialogue: 0,0:00:03"), "no clipB captions without a clipB transcript:\n{doc}");
    }

    #[test]
    fn all_sourced_plan_captions_map_fine() {
        // Every segment is an insert (zero `source: None`). The MAIN input is only a normalization
        // reference; captions still map correctly for each insert source.
        let probe = probe_16x9();
        let segs = vec![
            Segment { start: 0.0, end: 2.0, source: Some("a.mp4".into()) },
            Segment { start: 0.0, end: 2.0, source: Some("b.mp4".into()) },
        ];
        let plan = EditPlan { segments: segs, captions: true, ..Default::default() };
        let mut tmap = TranscriptMap::default();
        tmap.insert(Some("a.mp4"), Transcript {
            text: "aye".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 2.0, text: "aye".into() }],
            words: vec![TranscriptWord { start: 0.5, end: 1.0, word: "aye".into() }],
        });
        tmap.insert(Some("b.mp4"), Transcript {
            text: "bee".into(),
            segments: vec![TranscriptSegment { start: 0.0, end: 2.0, text: "bee".into() }],
            words: vec![TranscriptWord { start: 0.5, end: 1.0, word: "bee".into() }],
        });
        let doc = build_ass_multi(&tmap, &plan, &probe);
        assert!(doc.contains("aye") && doc.contains("bee"));
        // a.mp4 word at 0.5 → out 0.5; b.mp4 word at 0.5 → offset 2 + 0.5 = 2.5.
        assert!(doc.contains("Dialogue: 0,0:00:00.50,"), "a at 0.5:\n{doc}");
        assert!(doc.contains("Dialogue: 0,0:00:02.50,"), "b at 2.5:\n{doc}");
    }

    // ---- compilation planner sanitizer ----

    #[test]
    fn sanitize_compilation_spec_valid_preserves_order() {
        let clips = vec![("a.mp4".to_string(), 30.0), ("b.mp4".to_string(), 40.0)];
        let spec = CompilationSpec {
            title: "  My Reel  ".into(),
            mode_detected: "MONTAGE".into(),
            notes: "n".into(),
            segments: vec![
                Segment { start: 1.0, end: 6.0, source: Some("a.mp4".into()) },
                Segment { start: 2.0, end: 9.0, source: Some("b.mp4".into()) },
            ],
        };
        let out = sanitize_compilation_spec(spec, &clips, Some(12.0)).unwrap();
        assert_eq!(out.title, "My Reel");
        assert_eq!(out.mode_detected, "montage");
        assert_eq!(out.segments.len(), 2);
        // Order is NOT sorted — flow order the model chose is preserved.
        assert_eq!(out.segments[0].source.as_deref(), Some("a.mp4"));
        assert_eq!(out.segments[1].source.as_deref(), Some("b.mp4"));
    }

    #[test]
    fn sanitize_compilation_spec_drops_unknown_tiny_and_clamps() {
        let clips = vec![("a.mp4".to_string(), 10.0)];
        let spec = CompilationSpec {
            segments: vec![
                Segment { start: 0.0, end: 0.2, source: Some("a.mp4".into()) },  // <0.5s → drop
                Segment { start: 5.0, end: 99.0, source: Some("a.mp4".into()) }, // clamp end to 10
                Segment { start: 1.0, end: 4.0, source: Some("ghost.mp4".into()) }, // unknown → drop
                Segment { start: 1.0, end: 4.0, source: None },                 // no source → drop
            ],
            ..Default::default()
        };
        let out = sanitize_compilation_spec(spec, &clips, None).unwrap();
        assert_eq!(out.segments.len(), 1);
        assert_eq!(out.segments[0].source.as_deref(), Some("a.mp4"));
        assert!((out.segments[0].end - 10.0).abs() < 1e-9, "end clamped to clip duration");
        assert_eq!(out.mode_detected, "montage"); // default when model gave nothing
        assert_eq!(out.title, "Compilation"); // default when blank
    }

    #[test]
    fn sanitize_compilation_spec_rejects_out_of_window_total() {
        let clips = vec![("a.mp4".to_string(), 100.0)];
        let spec = CompilationSpec {
            segments: vec![Segment { start: 0.0, end: 5.0, source: Some("a.mp4".into()) }],
            ..Default::default()
        };
        // Target 60 → window 30–120s; a 5s total is below → rejected (triggers a re-prompt upstream).
        assert!(sanitize_compilation_spec(spec, &clips, Some(60.0)).is_err());
    }

    #[test]
    fn sanitize_compilation_spec_empty_errs_and_takes_normalizes() {
        let clips = vec![("a.mp4".to_string(), 10.0)];
        // All segments dropped ⇒ error.
        let empty = CompilationSpec { segments: vec![], ..Default::default() };
        assert!(sanitize_compilation_spec(empty, &clips, None).is_err());
        // "Takes" (any case / synonym) normalizes to "takes".
        let spec = CompilationSpec {
            mode_detected: "Takes".into(),
            segments: vec![Segment { start: 0.0, end: 5.0, source: Some("a.mp4".into()) }],
            ..Default::default()
        };
        let out = sanitize_compilation_spec(spec, &clips, None).unwrap();
        assert_eq!(out.mode_detected, "takes");
    }
}
