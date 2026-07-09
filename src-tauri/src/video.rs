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

use crate::db::{first_non_empty_setting, get_app_setting_value};
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
            segments.push(Segment { start: cursor, end });
        }
        cursor = (s.end - pad_s).max(cursor);
    }
    if duration_s - cursor > 0.05 {
        segments.push(Segment { start: cursor, end: duration_s });
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
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
            s.start = s.start.max(0.0);
            s.end = s.end.min(probe.duration_s + 0.5);
            if s.end - s.start < 0.05 {
                return Err(format!("segment {:.2}–{:.2}s is empty or inverted", s.start, s.end));
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
        vec![Segment { start: 0.0, end: probe.duration_s }]
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

/// Map a SOURCE-timeline interval to the OUTPUT timeline for the given kept segments and
/// speed. Returns `(out_start, out_end, segment_index)`. Intervals that fall entirely
/// outside every kept segment return `None` (the word is dropped); partially-clipped
/// intervals are clamped to the kept segment they most overlap.
fn map_interval_to_output(
    ws: f64,
    we: f64,
    segments: &[Segment],
    speed: f64,
) -> Option<(f64, f64, usize)> {
    let speed = if speed > 0.0 { speed } else { 1.0 };
    let offs = segment_offsets(segments);
    let mut best: Option<(usize, f64)> = None;
    for (i, s) in segments.iter().enumerate() {
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

/// Group a transcript's words into short caption lines (~3–5 words / ~28 chars), breaking
/// at speech gaps > 0.8s, transcript-segment boundaries, or edit cuts, and mapping each
/// word onto the OUTPUT timeline. Falls back to segment-level lines when the transcript
/// has no word timings. Words outside the kept segments are dropped.
fn group_caption_lines(transcript: &Transcript, segments: &[Segment], speed: f64) -> Vec<CaptionLine> {
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
        let (os, oe, eseg) = match map_interval_to_output(it.start, it.end, segments, speed) {
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

/// Build a full ASS subtitle document for the plan's captions. Pure/unit-testable. Times
/// are on the OUTPUT timeline and PlayResX/Y match the OUTPUT resolution so styling scales.
pub(crate) fn build_ass(transcript: &Transcript, plan: &EditPlan, probe: &VideoProbe) -> String {
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

    let lines = group_caption_lines(transcript, &effective_segments(plan, probe), plan.speed.unwrap_or(1.0));
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

/// Build an SRT or VTT document from a transcript, optionally applying an edit plan's
/// timeline mapping (empty/absent plan = source timeline). Pure/unit-testable.
pub(crate) fn build_subtitles(
    transcript: &Transcript,
    format: &str,
    plan: Option<&EditPlan>,
    probe: &VideoProbe,
) -> String {
    let (segments, speed) = match plan {
        Some(p) => (effective_segments(p, probe), p.speed.unwrap_or(1.0)),
        None => (vec![Segment { start: 0.0, end: probe.duration_s }], 1.0),
    };
    let lines = group_caption_lines(transcript, &segments, speed);
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

/// Build the full ffmpeg argument list for a plan. Pure so it can be unit-tested.
/// `text_files` must contain one temp-file path per entry in `plan.texts` (drawtext
/// reads the text from a file, which sidesteps filter-graph escaping entirely).
/// `ass_file`, when present, burns those subtitles in after the crop stage.
pub(crate) fn build_render_args(
    src: &Path,
    plan: &EditPlan,
    probe: &VideoProbe,
    font: &str,
    text_files: &[PathBuf],
    ass_file: Option<&Path>,
    encoder: VideoEncoder,
    out: &Path,
) -> Result<Vec<String>, String> {
    let with_audio = probe.has_audio && !plan.mute;
    let segments: Vec<Segment> = if plan.segments.is_empty() {
        vec![Segment { start: 0.0, end: probe.duration_s }]
    } else {
        plan.segments.clone()
    };

    // De-click micro-fades: with 2+ segments, concat stitches independently-decoded audio
    // clips together, and a hard cut mid-waveform is audible as a click/pop. A ~20ms
    // in/out fade on each clip (clamped to half its own duration for very short segments)
    // fixes that with no new EditPlan field — it's always on whenever there's a cut.
    const MICRO_FADE_S: f64 = 0.02;
    let apply_microfades = with_audio && segments.len() > 1;

    let mut graph = String::new();
    for (i, s) in segments.iter().enumerate() {
        graph.push_str(&format!(
            "[0:v]trim=start={:.4}:end={:.4},setpts=PTS-STARTPTS[v{i}];",
            s.start, s.end
        ));
        if with_audio {
            let seg_dur = (s.end - s.start).max(0.0);
            graph.push_str(&format!(
                "[0:a]atrim=start={:.4}:end={:.4},asetpts=PTS-STARTPTS",
                s.start, s.end
            ));
            // Denoise per kept segment (simpler and just as effective as denoising the
            // whole track once, without needing an explicit asplit to fan `[0:a]` out to
            // both a denoise stage and the per-segment atrims).
            if plan.denoise {
                graph.push_str(",afftdn=nf=-25");
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
        post.push(format!("crop=min(iw\\,ih*{aw}/{ah}):min(ih\\,iw*{ah}/{aw})"));
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
        "-i".into(),
        src.to_string_lossy().to_string(),
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "[vout]".into(),
    ];
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

fn sanitize_stem(name: &str) -> String {
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
    transcript: Option<&Transcript>,
    out_dir: &Path,
    prefer_hw_encode: bool,
) -> Result<PathBuf, String> {
    let probe = probe_video(src)?;
    let mut plan = plan.clone();
    plan.validate(&probe)?;

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
        match transcript {
            Some(t) if !(t.words.is_empty() && t.segments.is_empty()) => {
                let p = text_dir.join("captions.ass");
                std::fs::write(&p, build_ass(t, &plan, &probe).as_bytes()).map_err(|e| e.to_string())?;
                Some(p)
            }
            _ => {
                let _ = std::fs::remove_dir_all(&text_dir);
                return Err(
                    "captions were requested but no transcript is available — transcribe the video first"
                        .to_string(),
                );
            }
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

    let encoder = pick_encoder(prefer_hw_encode);
    let mut used_encoder = encoder;
    let args = build_render_args(src, &plan, &probe, font, &text_files, ass_file.as_deref(), encoder, &out_path)?;
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
            src,
            &plan,
            &probe,
            font,
            &text_files,
            ass_file.as_deref(),
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
fn transcribe_local(audio: &Path) -> Result<Transcript, String> {
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
    let (api_key, model, endpoint) =
        if let Some(key) = first_non_empty_setting(state, &["llm/groq_api_key"])
            .map_err(|e| e.to_string())?
            .or_else(|| std::env::var("GROQ_API_KEY").ok())
        {
            (key, "whisper-large-v3-turbo", "https://api.groq.com/openai/v1/audio/transcriptions")
        } else if let Some(key) = get_app_setting_value(state, "llm/openai_api_key")?
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        {
            (key, "whisper-1", "https://api.openai.com/v1/audio/transcriptions")
        } else {
            return Err("No transcription API key configured. Set llm/groq_api_key (or llm/openai_api_key) in settings, or enable on-device transcription.".to_string());
        };

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
        let mut p = EditPlan { segments: vec![Segment { start: 5.0, end: 4.0 }], ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut p = EditPlan { filter: Some("sepia-dreams".into()), ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut p = EditPlan { speed: Some(9.0), ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut p = EditPlan { aspect: Some("9x16".into()), ..Default::default() };
        assert!(p.validate(&probe).is_err());
        let mut ok = EditPlan {
            segments: vec![Segment { start: 0.0, end: 10.0 }],
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
            segments: vec![Segment { start: 1.0, end: 4.0 }, Segment { start: 7.0, end: 11.0 }],
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
        let args = build_render_args(
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
            segments: vec![Segment { start: 1.0, end: 3.0 }, Segment { start: 5.0, end: 8.0 }],
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
        };
        plan.validate(&probe).unwrap();
        let out = dir.join("out.mp4");
        let args =
            build_render_args(&src, &plan, &probe, font_path().unwrap(), &[text_file], None, VideoEncoder::Libx264, &out)
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
        let cap_args = build_render_args(
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
            segments: vec![Segment { start: 0.0, end: 10.0 }, Segment { start: 20.0, end: 30.0 }],
            ..Default::default()
        };
        let args = build_render_args(
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
        let args = build_render_args(
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
        let args = build_render_args(
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
            segments: vec![Segment { start: 0.0, end: 8.0 }],
            speed: Some(2.0),
            fade: Some(1.0),
            ..Default::default()
        };
        let args = build_render_args(
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
        let one_seg = EditPlan { segments: vec![Segment { start: 1.0, end: 10.0 }], ..Default::default() };
        let args = build_render_args(
            Path::new("/tmp/in.mp4"), &one_seg, &probe, "/tmp/f.ttc", &[], None, VideoEncoder::Libx264, Path::new("/tmp/out.mp4"),
        ).unwrap();
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(!graph.contains("afade"), "no cuts, no micro-fades expected: {graph}");

        // Two segments: a concat happens, so each per-segment audio clip gets a ~20ms
        // in/out afade to avoid an audible click at the cut.
        let two_segs = EditPlan {
            segments: vec![Segment { start: 1.0, end: 5.0 }, Segment { start: 10.0, end: 15.0 }],
            ..Default::default()
        };
        let args = build_render_args(
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
        let segs = vec![Segment { start: 0.0, end: 10.0 }];
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
        let segs = vec![Segment { start: 1.0, end: 4.0 }, Segment { start: 7.0, end: 11.0 }];
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
        let segs = vec![Segment { start: 0.0, end: 5.0 }, Segment { start: 10.0, end: 15.0 }];
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
        let segs = vec![Segment { start: 0.0, end: 6.0 }];
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
        let plan = EditPlan { segments: vec![Segment { start: 0.0, end: 2.0 }], speed: Some(2.0), ..Default::default() };
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
        let args = build_render_args(
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
        let args = build_render_args(
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
            build_render_args(&src, &plan, &probe, font_path().unwrap(), &[], None, encoder, &out).unwrap();
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
}
