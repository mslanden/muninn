//! macOS screen capture + app launching for the `capture_screen` / `open_app` tools.
//!
//! Capture uses the system `screencapture` binary; "window" mode reads the frontmost
//! window's bounds via System Events (requires Accessibility permission) and captures that
//! rectangle, falling back to a full-screen grab when bounds aren't available. Capturing the
//! screen requires the app to hold macOS **Screen Recording** permission — the first capture
//! triggers the system prompt; until granted, the image is desktop-only.

use std::process::Command;

use base64::{engine::general_purpose::STANDARD as B64, Engine};

/// Capture the screen and return (media_type, base64_png, human_note).
/// `mode` is "screen" (whole main display, default) or "window" (frontmost window).
pub fn capture_screen(mode: &str) -> Result<(String, String, String), String> {
    if !cfg!(target_os = "macos") {
        return Err("Screen capture is only supported on macOS.".to_string());
    }

    let out_path = std::env::temp_dir().join(format!("muninn-capture-{}.png", std::process::id()));
    let out = out_path.to_string_lossy().to_string();

    let (rect, note) = if mode == "window" {
        match frontmost_window_rect() {
            Some(r) => (Some(r), "frontmost window".to_string()),
            None => (
                None,
                "frontmost window bounds unavailable (grant Accessibility permission for precise window capture) — captured the full screen instead".to_string(),
            ),
        }
    } else {
        (None, "full screen".to_string())
    };

    // -x = no capture sound. -o = no window shadow. -R = capture a rectangle.
    let mut cmd = Command::new("screencapture");
    cmd.arg("-x");
    if let Some((x, y, w, h)) = rect {
        cmd.arg("-R").arg(format!("{},{},{},{}", x, y, w, h));
    }
    cmd.arg(&out);

    let status = cmd.status().map_err(|e| format!("failed to run screencapture: {}", e))?;
    if !status.success() {
        return Err("screencapture exited with an error (is Screen Recording permission granted in System Settings → Privacy & Security?)".to_string());
    }

    let bytes = std::fs::read(&out_path).map_err(|e| format!("failed to read capture: {}", e))?;
    let _ = std::fs::remove_file(&out_path);
    if bytes.is_empty() {
        return Err("capture produced an empty image".to_string());
    }
    let b64 = B64.encode(&bytes);
    Ok(("image/png".to_string(), b64, format!("Captured {} ({} KB).", note, bytes.len() / 1024)))
}

/// One on-screen (or off-screen) application window, as reported by the window server.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub owner: String,
    pub id: u64,
    pub title: String,
    pub width: f64,
    pub height: f64,
}

/// List normal application windows (layer 0) front-to-back via CoreGraphics, without sending
/// Apple events or touching focus. `on_screen_only: false` also returns windows on other
/// Spaces and hidden/minimized ones.
fn list_windows(on_screen_only: bool) -> Result<Vec<WindowInfo>, String> {
    // JXA so no Accessibility/Automation permission is involved: this only reads the window
    // list. (Window titles come back empty without Screen Recording permission.)
    let script = format!(
        r#"ObjC.import("CoreGraphics");
var opts = {} | $.kCGWindowListExcludeDesktopElements;
var list = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo(opts, 0))) || [];
JSON.stringify(list.filter(function (w) {{ return w.kCGWindowLayer === 0; }}).map(function (w) {{
  var b = w.kCGWindowBounds || {{}};
  return {{ owner: w.kCGWindowOwnerName || "", id: w.kCGWindowNumber, title: w.kCGWindowName || "", width: b.Width || 0, height: b.Height || 0 }};
}}));"#,
        if on_screen_only { "$.kCGWindowListOptionOnScreenOnly" } else { "$.kCGWindowListOptionAll" }
    );
    let output = Command::new("osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| format!("failed to run osascript: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "could not list windows: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("could not parse window list: {}", e))?;
    Ok(parsed
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|w| {
                    Some(WindowInfo {
                        owner: w.get("owner")?.as_str()?.to_string(),
                        id: w.get("id")?.as_u64()?,
                        title: w.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                        width: w.get("width").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        height: w.get("height").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Pick the window to capture for `app`: an exact (case-insensitive) app-name match wins over
/// a partial one, and among an app's windows the largest wins (skips palettes and popovers).
fn pick_window<'a>(windows: &'a [WindowInfo], app: &str) -> Option<&'a WindowInfo> {
    let want = app.trim().to_lowercase();
    if want.is_empty() {
        return None;
    }
    let usable = |w: &&WindowInfo| w.width >= 80.0 && w.height >= 80.0;
    let largest = |matches: Vec<&'a WindowInfo>| {
        matches.into_iter().max_by(|a, b| {
            (a.width * a.height)
                .partial_cmp(&(b.width * b.height))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    };
    let exact: Vec<&WindowInfo> = windows.iter().filter(usable).filter(|w| w.owner.to_lowercase() == want).collect();
    if !exact.is_empty() {
        return largest(exact);
    }
    largest(windows.iter().filter(usable).filter(|w| w.owner.to_lowercase().contains(&want)).collect())
}

/// Screenshot one app's window **without bringing it to the front** — it can be behind other
/// windows. Returns (media_type, base64_png, human_note). Needs Screen Recording permission,
/// like every capture.
pub fn capture_app_window(app: &str) -> Result<(String, String, String), String> {
    if !cfg!(target_os = "macos") {
        return Err("Screen capture is only supported on macOS.".to_string());
    }
    let on_screen = list_windows(true)?;
    let (window, off_screen) = match pick_window(&on_screen, app) {
        Some(w) => (w.clone(), false),
        None => {
            let all = list_windows(false).unwrap_or_default();
            match pick_window(&all, app) {
                Some(w) => (w.clone(), true),
                None => {
                    let mut names: Vec<String> = on_screen.iter().map(|w| w.owner.clone()).collect();
                    names.sort();
                    names.dedup();
                    return Err(format!(
                        "No window found for app '{}'. Apps with a window on screen right now: {}. If the app isn't running, start it first (open_app with background: true keeps it out of the user's way).",
                        app,
                        if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
                    ));
                }
            }
        }
    };

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out_path = std::env::temp_dir().join(format!("muninn-capture-{}-{}.png", std::process::id(), nanos));
    // -x no sound, -o no window shadow, -l capture this window id even if it is covered.
    let status = Command::new("screencapture")
        .arg("-x")
        .arg("-o")
        .arg("-l")
        .arg(window.id.to_string())
        .arg(&out_path)
        .status()
        .map_err(|e| format!("failed to run screencapture: {}", e))?;
    let bytes = std::fs::read(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    if !status.success() || bytes.is_empty() {
        return Err(format!(
            "could not capture the {} window{} (is Screen Recording permission granted in System Settings → Privacy & Security?)",
            window.owner,
            if off_screen { " — it is minimized, hidden or on another Space, which macOS often refuses to capture" } else { "" }
        ));
    }
    let title = if window.title.is_empty() { String::new() } else { format!(" \"{}\"", window.title) };
    let note = format!(
        "Captured the {} window{} in the background, without bringing it to the front ({} KB).{}",
        window.owner,
        title,
        bytes.len() / 1024,
        if off_screen { " Note: the window was not on screen (minimized, hidden or on another Space), so the image may be stale." } else { "" }
    );
    Ok(("image/png".to_string(), B64.encode(&bytes), note))
}

/// Longest edge (px) an image is sent to the model at. Larger images are downscaled first:
/// a full-resolution photo or Retina screenshot costs far more tokens without reading better.
const MODEL_IMAGE_MAX_EDGE: u32 = 1568;
/// Largest image file `read_file` will load at all.
const MODEL_IMAGE_MAX_FILE_BYTES: u64 = 40 * 1024 * 1024;
/// Largest payload sent as-is when it can't be downscaled. Providers cap an image at 5 MB
/// of base64 (Anthropic: 5,242,880), which is ~3.9 MB of raw bytes after the 4/3 expansion.
const MODEL_IMAGE_MAX_RAW_BYTES: usize = 3_750_000;

/// If `path` has a raster-image extension the model can be shown, return whether the format
/// can be sent as-is (`Some(media_type)`) or must be converted first (`None`). Returns `None`
/// overall for anything else — including SVG, which is text and should be read as text.
pub fn image_kind(path: &std::path::Path) -> Option<Option<&'static str>> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some(Some("image/png")),
        "jpg" | "jpeg" => Some(Some("image/jpeg")),
        "gif" => Some(Some("image/gif")),
        "webp" => Some(Some("image/webp")),
        "heic" | "heif" | "bmp" | "tif" | "tiff" => Some(None),
        _ => None,
    }
}

/// Load an image file for a vision model: returns (media_type, base64, human_note).
/// Downscales to `MODEL_IMAGE_MAX_EDGE` and converts unsupported formats via the system
/// `sips` tool; when `sips` is unavailable a small image in a supported format is sent as-is.
/// The caller is responsible for workspace-scoping `path`.
pub fn load_image_for_model(path: &std::path::Path) -> Result<(String, String, String), String> {
    let native = image_kind(path).ok_or_else(|| "not a supported image type".to_string())?;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MODEL_IMAGE_MAX_FILE_BYTES {
        return Err(format!(
            "image is too large to load ({} MB; limit {} MB)",
            meta.len() / (1024 * 1024),
            MODEL_IMAGE_MAX_FILE_BYTES / (1024 * 1024)
        ));
    }

    let dims = image_dimensions(path);
    let oversized = dims.map(|(w, h)| w.max(h) > MODEL_IMAGE_MAX_EDGE).unwrap_or(false);
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let dims_text = dims.map(|(w, h)| format!("{}x{}", w, h)).unwrap_or_else(|| "unknown size".to_string());

    if oversized || native.is_none() {
        // JPEG stays JPEG (photos); everything else becomes PNG (crisp UI text).
        let (format, media_type, ext) = if native == Some("image/jpeg") {
            ("jpeg", "image/jpeg", "jpg")
        } else {
            ("png", "image/png", "png")
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!("muninn-img-{}-{}.{}", std::process::id(), nanos, ext));
        let mut cmd = Command::new("sips");
        if oversized {
            cmd.arg("-Z").arg(MODEL_IMAGE_MAX_EDGE.to_string());
        }
        let converted = cmd
            .arg("-s").arg("format").arg(format)
            .arg(path)
            .arg("--out").arg(&tmp)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|_| std::fs::read(&tmp).ok())
            .filter(|b| !b.is_empty());
        let _ = std::fs::remove_file(&tmp);
        if let Some(bytes) = converted {
            let note = format!(
                "Loaded image {} ({}{}, {} KB sent). The image is attached to this result.",
                name,
                dims_text,
                if oversized { format!(", downscaled to fit {}px", MODEL_IMAGE_MAX_EDGE) } else { String::new() },
                bytes.len() / 1024
            );
            return Ok((media_type.to_string(), B64.encode(&bytes), note));
        }
        if native.is_none() {
            return Err(format!("could not convert {} to a format the model can view", name));
        }
        // Resize failed but the format is viewable — fall through and try sending it as-is.
    }

    let media_type = native.ok_or_else(|| "unsupported image format".to_string())?;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("image file is empty".to_string());
    }
    if bytes.len() > MODEL_IMAGE_MAX_RAW_BYTES {
        return Err(format!(
            "image is too large to send ({} KB) and could not be downscaled",
            bytes.len() / 1024
        ));
    }
    let note = format!(
        "Loaded image {} ({}, {} KB). The image is attached to this result.",
        name,
        dims_text,
        bytes.len() / 1024
    );
    Ok((media_type.to_string(), B64.encode(&bytes), note))
}

/// Pixel dimensions of an image via `sips`, or None when unavailable/unreadable.
fn image_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    let output = Command::new("sips")
        .arg("-g").arg("pixelWidth")
        .arg("-g").arg("pixelHeight")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |key: &str| -> Option<u32> {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .and_then(|v| v.trim().parse().ok())
    };
    Some((field("pixelWidth:")?, field("pixelHeight:")?))
}

/// Read the frontmost (focused) window's bounds as (x, y, w, h) via System Events.
/// Returns None if Accessibility permission is missing or there's no front window.
fn frontmost_window_rect() -> Option<(i64, i64, i64, i64)> {
    // Returns e.g. "12, 47, 1200, 800" — position then size of the front window.
    let script = r#"tell application "System Events" to tell (first process whose frontmost is true)
        set p to position of front window
        set s to size of front window
        return (item 1 of p as string) & "," & (item 2 of p as string) & "," & (item 1 of s as string) & "," & (item 2 of s as string)
    end tell"#;
    let output = Command::new("osascript").arg("-e").arg(script).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let nums: Vec<i64> = text
        .trim()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    if nums.len() == 4 && nums[2] > 0 && nums[3] > 0 {
        Some((nums[0], nums[1], nums[2], nums[3]))
    } else {
        None
    }
}

/// Launch an app, optionally opening a URL in it. By default it is brought to the front (so a
/// subsequent `capture_screen` mode:"window" grabs it); with `background` it is opened behind
/// the user's current window without taking focus — pair that with `capture_app_window`.
/// Returns a human-readable note.
pub fn open_app(app: &str, url: Option<&str>, background: bool) -> Result<String, String> {
    if !cfg!(target_os = "macos") {
        return Err("open_app is only supported on macOS.".to_string());
    }
    let app = app.trim();
    if app.is_empty() {
        return Err("missing app name".to_string());
    }

    // `open -a "<App>" "<url>"` launches/activates the app, opening the URL when given.
    let mut cmd = Command::new("open");
    if background {
        // -g: do not bring the application to the foreground.
        cmd.arg("-g");
    }
    cmd.arg("-a").arg(app);
    if let Some(u) = url.map(str::trim).filter(|u| !u.is_empty()) {
        cmd.arg(u);
    }
    let status = cmd.status().map_err(|e| format!("failed to run open: {}", e))?;
    if !status.success() {
        return Err(format!("could not open app '{}' (is it installed? use its exact name, e.g. \"Google Chrome\")", app));
    }

    // Give the window a moment to appear before any follow-up capture.
    std::thread::sleep(std::time::Duration::from_millis(900));

    let how = if background {
        "in the background (focus was not changed)"
    } else {
        "and brought it to the front"
    };
    Ok(match url {
        Some(u) if !u.trim().is_empty() => format!("Opened {} in {} {}.", u.trim(), app, how),
        _ => format!("Launched {} {}.", app, how),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A valid 1x1 PNG.
    const TINY_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muninn-screen-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn win(owner: &str, id: u64, w: f64, h: f64) -> WindowInfo {
        WindowInfo { owner: owner.to_string(), id, title: String::new(), width: w, height: h }
    }

    #[test]
    fn pick_window_prefers_exact_name_and_largest_window() {
        let windows = vec![
            win("Google Chrome Helper", 1, 2000.0, 2000.0),
            win("Google Chrome", 2, 300.0, 200.0),
            win("Google Chrome", 3, 1200.0, 800.0),
            win("Safari", 4, 1000.0, 700.0),
            win("Safari", 5, 20.0, 20.0),
        ];
        assert_eq!(pick_window(&windows, "google chrome").map(|w| w.id), Some(3));
        assert_eq!(pick_window(&windows, " Safari ").map(|w| w.id), Some(4));
        // Partial match only when nothing matches exactly.
        assert_eq!(pick_window(&windows, "chrome").map(|w| w.id), Some(1));
        assert_eq!(pick_window(&windows, "Xcode"), None);
        assert_eq!(pick_window(&windows, ""), None);
    }

    /// Needs a logged-in GUI session with Screen Recording permission:
    /// `cargo test --lib screen -- --ignored`.
    #[test]
    #[ignore]
    fn captures_a_window_in_the_background() {
        let windows = list_windows(true).unwrap();
        let target = windows
            .iter()
            .find(|w| w.width >= 200.0 && w.height >= 200.0)
            .expect("no on-screen window to capture");
        let (media, data, note) = capture_app_window(&target.owner).unwrap();
        assert_eq!(media, "image/png");
        assert!(B64.decode(data).unwrap().len() > 1000);
        assert!(note.contains("without bringing it to the front"), "{}", note);
        assert!(capture_app_window("No Such App 12345").unwrap_err().contains("No window found"));
    }

    #[test]
    fn image_kind_classifies_by_extension() {
        assert_eq!(image_kind(Path::new("a/shot.PNG")), Some(Some("image/png")));
        assert_eq!(image_kind(Path::new("photo.jpeg")), Some(Some("image/jpeg")));
        assert_eq!(image_kind(Path::new("anim.gif")), Some(Some("image/gif")));
        assert_eq!(image_kind(Path::new("pic.webp")), Some(Some("image/webp")));
        // Viewable only after conversion.
        assert_eq!(image_kind(Path::new("IMG_0001.heic")), Some(None));
        // Text and everything else stay on the text path.
        assert_eq!(image_kind(Path::new("logo.svg")), None);
        assert_eq!(image_kind(Path::new("main.rs")), None);
        assert_eq!(image_kind(Path::new("README")), None);
    }

    #[test]
    fn small_png_is_loaded_as_is() {
        let path = temp_file("tiny.png");
        let bytes = B64.decode(TINY_PNG_B64).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let (media_type, data, note) = load_image_for_model(&path).unwrap();
        assert_eq!(media_type, "image/png");
        assert_eq!(B64.decode(data).unwrap(), bytes);
        assert!(note.contains("tiny.png"));
    }

    #[test]
    fn empty_image_is_an_error() {
        let path = temp_file("empty.png");
        std::fs::write(&path, b"").unwrap();
        assert!(load_image_for_model(&path).is_err());
    }

    #[test]
    fn oversized_image_is_downscaled() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let small = temp_file("seed.png");
        std::fs::write(&small, B64.decode(TINY_PNG_B64).unwrap()).unwrap();
        let big = temp_file("big.png");
        // Stretch the seed to 3000x2000 so it exceeds the max edge.
        let made = Command::new("sips")
            .args(["-z", "2000", "3000"])
            .arg(&small)
            .arg("--out")
            .arg(&big)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            return;
        }
        assert_eq!(image_dimensions(&big), Some((3000, 2000)));
        let (media_type, data, note) = load_image_for_model(&big).unwrap();
        assert_eq!(media_type, "image/png");
        assert!(note.contains("downscaled"), "note was: {}", note);
        let out = temp_file("big-out.png");
        std::fs::write(&out, B64.decode(data).unwrap()).unwrap();
        assert_eq!(image_dimensions(&out), Some((MODEL_IMAGE_MAX_EDGE, 1045)));
    }
}
