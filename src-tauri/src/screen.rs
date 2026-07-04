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

/// Launch and/or focus an app, optionally opening a URL in it (brings it to the front so a
/// subsequent `capture_screen` mode:"window" grabs it). Returns a human-readable note.
pub fn open_app(app: &str, url: Option<&str>) -> Result<String, String> {
    if !cfg!(target_os = "macos") {
        return Err("open_app is only supported on macOS.".to_string());
    }
    let app = app.trim();
    if app.is_empty() {
        return Err("missing app name".to_string());
    }

    // `open -a "<App>" "<url>"` launches/activates the app, opening the URL when given.
    let mut cmd = Command::new("open");
    cmd.arg("-a").arg(app);
    if let Some(u) = url.map(str::trim).filter(|u| !u.is_empty()) {
        cmd.arg(u);
    }
    let status = cmd.status().map_err(|e| format!("failed to run open: {}", e))?;
    if !status.success() {
        return Err(format!("could not open app '{}' (is it installed? use its exact name, e.g. \"Google Chrome\")", app));
    }

    // Give the window a moment to come to the front before any follow-up capture.
    std::thread::sleep(std::time::Duration::from_millis(900));

    Ok(match url {
        Some(u) if !u.trim().is_empty() => format!("Opened {} in {} and brought it to the front.", u.trim(), app),
        _ => format!("Launched/activated {} and brought it to the front.", app),
    })
}
