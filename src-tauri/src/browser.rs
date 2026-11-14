//! Headless-browser page capture for the `capture_page` tool.
//!
//! Loads a URL in a Chromium-based browser that is already installed (Chrome, Chromium, Brave
//! or Edge) with `--headless`, so nothing appears on screen and no window is focused, and
//! returns three things from one run: a screenshot, everything the page wrote to the console
//! (including uncaught errors), and the rendered DOM as text. Each run uses a throwaway
//! profile directory so it never touches — or attaches to — the user's own running browser.
//!
//! Target validation (which URLs may be loaded) is the caller's job; see
//! `tools::resolve_page_target`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Hard ceiling on top of the page's own wait budget before the browser is killed.
const HARD_TIMEOUT_EXTRA_MS: u64 = 25_000;
const MAX_CONSOLE_LINES: usize = 40;
const MAX_CONSOLE_CHARS: usize = 4_000;
const MAX_TEXT_CHARS: usize = 6_000;

const BROWSERS: &[(&str, &str)] = &[
    ("Google Chrome", "Google Chrome.app/Contents/MacOS/Google Chrome"),
    ("Chromium", "Chromium.app/Contents/MacOS/Chromium"),
    ("Brave Browser", "Brave Browser.app/Contents/MacOS/Brave Browser"),
    ("Microsoft Edge", "Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
    ("Google Chrome Canary", "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"),
];

/// First installed Chromium-based browser: (display name, executable).
pub fn find_browser() -> Option<(&'static str, PathBuf)> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
    }
    for (name, rel) in BROWSERS {
        for root in &roots {
            let exe = root.join(rel);
            if exe.is_file() {
                return Some((name, exe));
            }
        }
    }
    None
}

pub struct PageCapture {
    /// (media_type, base64) of the screenshot, when one was produced.
    pub image: Option<(String, String)>,
    /// Text report for the model: title, console output, page text.
    pub report: String,
}

/// Load `url` headlessly at `width`x`height`, letting the page run for `wait_ms` of its own
/// time (timers, fetches) before capturing.
pub async fn capture_page(url: &str, width: u32, height: u32, wait_ms: u64) -> Result<PageCapture, String> {
    let (browser_name, exe) = find_browser().ok_or_else(|| {
        "No Chromium-based browser found. capture_page needs Google Chrome, Chromium, Brave or Microsoft Edge installed in /Applications.".to_string()
    })?;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let work = std::env::temp_dir().join(format!("muninn-page-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let result = run_capture(browser_name, &exe, &work, url, width, height, wait_ms).await;
    let _ = std::fs::remove_dir_all(&work);
    result
}

async fn run_capture(
    browser_name: &str,
    exe: &Path,
    work: &Path,
    url: &str,
    width: u32,
    height: u32,
    wait_ms: u64,
) -> Result<PageCapture, String> {
    let shot = work.join("shot.png");
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .arg("--hide-scrollbars")
        .arg("--mute-audio")
        .arg(format!("--user-data-dir={}", work.join("profile").display()))
        .arg(format!("--window-size={},{}", width, height))
        .arg(format!("--virtual-time-budget={}", wait_ms))
        // Console messages and load failures are written to stderr with this on.
        .arg("--enable-logging=stderr")
        .arg("--v=0")
        .arg(format!("--screenshot={}", shot.display()))
        .arg("--dump-dom")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let hard = Duration::from_millis(wait_ms + HARD_TIMEOUT_EXTRA_MS);
    let output = match tokio::time::timeout(hard, cmd.output()).await {
        Ok(result) => result.map_err(|e| format!("failed to launch {}: {}", browser_name, e))?,
        // Dropping the future kills the browser (kill_on_drop).
        Err(_) => {
            return Err(format!(
                "{} did not finish loading {} within {}s and was stopped. The page may never go idle (long polling, an endless loader) — try a smaller wait_ms, or check the server's output.",
                browser_name,
                url,
                hard.as_secs()
            ))
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    let dom = String::from_utf8_lossy(&output.stdout);
    let log = parse_browser_log(&stderr);

    if let Some(failure) = &log.load_failure {
        let hint = if failure.contains("ERR_CONNECTION_REFUSED") {
            " Nothing is listening at that address — is the dev server running and ready? (process action \"output\" with wait_for shows its log.)"
        } else {
            ""
        };
        return Err(format!("Page failed to load: {}.{}", failure, hint));
    }

    let image = if shot.is_file() {
        crate::screen::load_image_for_model(&shot).ok().map(|(media, data, _)| (media, data))
    } else {
        None
    };
    if image.is_none() && dom.trim().is_empty() {
        return Err(format!(
            "{} produced no screenshot and no page content for {} (exit status: {}).",
            browser_name, url, output.status
        ));
    }

    let mut report = format!(
        "Captured {} at {}x{} in headless {} (nothing was shown on screen).{}",
        url,
        width,
        height,
        browser_name,
        if image.is_some() { " The screenshot is attached to this result." } else { " No screenshot was produced; text only." }
    );
    if let Some(title) = extract_title(&dom) {
        report.push_str(&format!("\nTitle: {}", title));
    }

    if log.console.is_empty() {
        report.push_str("\n\nConsole: no messages.");
    } else {
        let errors = log.console.iter().filter(|m| is_error_message(m)).count();
        report.push_str(&format!(
            "\n\nConsole ({} message{}{}):",
            log.console.len(),
            if log.console.len() == 1 { "" } else { "s" },
            if errors > 0 { format!(", {} uncaught error{}", errors, if errors == 1 { "" } else { "s" }) } else { String::new() }
        ));
        let mut used = 0usize;
        for (i, msg) in log.console.iter().enumerate() {
            if i >= MAX_CONSOLE_LINES || used >= MAX_CONSOLE_CHARS {
                report.push_str(&format!("\n- ...and {} more", log.console.len() - i));
                break;
            }
            let line = crate::tools::truncate_chars(msg, 600);
            used += line.len();
            report.push_str(&format!("\n- {}", line));
        }
    }

    let text = crate::tools::strip_html_tags(&dom);
    if text.is_empty() {
        report.push_str("\n\nPage text: (none — the page rendered no text)");
    } else {
        report.push_str(&format!("\n\nPage text:\n{}", crate::tools::truncate_chars(&text, MAX_TEXT_CHARS)));
    }

    Ok(PageCapture { image, report })
}

fn is_error_message(msg: &str) -> bool {
    msg.starts_with("Uncaught") || msg.starts_with("Unhandled")
}

#[derive(Default, Debug)]
struct BrowserLog {
    /// Console messages in order, each formatted `message (source:line)`.
    console: Vec<String>,
    /// Set when the top-level navigation failed, e.g. `net::ERR_CONNECTION_REFUSED`.
    load_failure: Option<String>,
}

/// Pull console messages and navigation failures out of Chromium's stderr log.
///
/// Console lines look like
/// `[pid:tid:date:INFO:CONSOLE:12] "message", source: http://host/app.js (12)`
/// (older builds write `INFO:CONSOLE(12)]`). A message containing newlines spans several log
/// lines, ending on the one that carries the `", source: ... (N)` suffix.
fn parse_browser_log(stderr: &str) -> BrowserLog {
    let mut log = BrowserLog::default();
    let mut pending: Option<String> = None;

    for line in stderr.lines() {
        if let Some(mut acc) = pending.take() {
            acc.push('\n');
            match split_console_tail(line) {
                Some((text, source)) => {
                    acc.push_str(text);
                    log.console.push(format_console(&acc, source));
                }
                None => {
                    acc.push_str(line);
                    pending = Some(acc);
                }
            }
            continue;
        }

        if let Some(rest) = console_line_body(line) {
            match split_console_tail(rest) {
                Some((text, source)) => log.console.push(format_console(text, source)),
                None => pending = Some(rest.to_string()),
            }
        } else if let Some(idx) = line.find("Page load failed: ") {
            if log.load_failure.is_none() {
                log.load_failure = Some(line[idx + "Page load failed: ".len()..].trim().to_string());
            }
        }
    }
    if let Some(acc) = pending {
        log.console.push(acc);
    }
    log
}

/// For a `...:CONSOLE:12] "message...` log line, the part after the opening quote.
fn console_line_body(line: &str) -> Option<&str> {
    if !line.starts_with('[') {
        return None;
    }
    let idx = line.find(":CONSOLE")?;
    let after = &line[idx + ":CONSOLE".len()..];
    let close = after.find("] \"")?;
    // Between `:CONSOLE` and `]` there is only the line number: `:12` or `(12)`.
    if !after[..close].chars().all(|c| c.is_ascii_digit() || matches!(c, ':' | '(' | ')')) {
        return None;
    }
    Some(&after[close + 3..])
}

/// Split `message", source: URL (N)` into (message, "URL:N").
fn split_console_tail(rest: &str) -> Option<(&str, String)> {
    let idx = rest.rfind("\", source: ")?;
    let tail = rest[idx + "\", source: ".len()..].trim_end();
    let open = tail.rfind(" (")?;
    let line_no = tail[open + 2..].strip_suffix(')')?;
    if line_no.is_empty() || !line_no.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let source = tail[..open].trim();
    let source = if source.is_empty() { String::new() } else { format!("{}:{}", source, line_no) };
    Some((&rest[..idx], source))
}

fn format_console(text: &str, source: String) -> String {
    if source.is_empty() {
        text.to_string()
    } else {
        format!("{} ({})", text, source)
    }
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let title = html.get(start..end)?.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_console_lines_and_ignores_noise() {
        let stderr = concat!(
            "[43435:2717824:1114/140011.216387:INFO:components/enterprise/x.cc:247] No machine level policy manager exists.\n",
            "[43441:2717937:1114/140011.534862:ERROR:ui/display/mac/cv_display_link_mac.mm:188] CVDisplayLinkCreateWithCGDisplay failed. CVReturn: -6670\n",
            "Trying to load the allocator multiple times. This is *not* supported.\n",
            "[43435:2717824:1114/140011.606040:INFO:CONSOLE:3] \"hello from console\", source: http://localhost:8731/ (3)\n",
            "[43435:2717824:1114/140011.622222:INFO:CONSOLE:4] \"Uncaught ReferenceError: undefinedFn is not defined\", source: http://localhost:8731/app.js (4)\n",
            "10882 bytes written to file /tmp/shot.png\n",
        );
        let log = parse_browser_log(stderr);
        assert!(log.load_failure.is_none());
        assert_eq!(
            log.console,
            vec![
                "hello from console (http://localhost:8731/:3)".to_string(),
                "Uncaught ReferenceError: undefinedFn is not defined (http://localhost:8731/app.js:4)".to_string(),
            ]
        );
        assert!(is_error_message(&log.console[1]));
        assert!(!is_error_message(&log.console[0]));
    }

    #[test]
    fn parses_old_format_multiline_and_quoted_messages() {
        let stderr = concat!(
            "[1:2:0101/000000.1:INFO:CONSOLE(7)] \"first line\n",
            "second \"quoted\" line\", source: http://x/a.js (7)\n",
            "[1:2:0101/000000.2:INFO:CONSOLE:9] \"no source\", source:  (9)\n",
        );
        let log = parse_browser_log(stderr);
        assert_eq!(
            log.console,
            vec![
                "first line\nsecond \"quoted\" line (http://x/a.js:7)".to_string(),
                "no source".to_string(),
            ]
        );
    }

    #[test]
    fn detects_load_failure() {
        let stderr = "[43501:2718655:1114/140023.450399:ERROR:components/headless/command_handler/headless_command_handler.cc:403] Page load failed: net::ERR_CONNECTION_REFUSED\n";
        let log = parse_browser_log(stderr);
        assert_eq!(log.load_failure.as_deref(), Some("net::ERR_CONNECTION_REFUSED"));
        assert!(log.console.is_empty());
    }

    /// End-to-end verification loop, exactly as the agent runs it: start a server in the
    /// background, wait for it, capture a page that logs and throws, stop the server.
    /// Needs python3 and a Chromium-based browser: `cargo test --lib browser -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn captures_a_local_dev_server_end_to_end() {
        let dir = std::env::temp_dir().join(format!("muninn-browser-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.html"),
            r#"<!doctype html><html><head><title>Probe Page</title><style>p{color:red}</style></head>
<body><h1>Hello probe</h1><p id="p">loading</p>
<script>console.log("hello from console");
setTimeout(function(){ document.getElementById("p").textContent = "loaded late"; undefinedFn(); }, 300);</script>
</body></html>"#,
        )
        .unwrap();

        let reg = crate::procs::ProcessRegistry::default();
        let id = reg
            .start("e2e", "python3 -u -m http.server 8734 --bind 127.0.0.1", &dir)
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let (found, running) = reg.log_contains(id, "serving http").unwrap();
            if found {
                break;
            }
            assert!(running, "server exited early: {}", reg.view(id, 2000).unwrap().output);
            assert!(std::time::Instant::now() < deadline, "server never became ready: {}", reg.view(id, 2000).unwrap().output);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let capture = capture_page("http://127.0.0.1:8734/", 1280, 800, 2000).await.unwrap();
        let (media, data) = capture.image.expect("screenshot");
        assert_eq!(media, "image/png");
        assert!(data.len() > 1000);
        let r = &capture.report;
        assert!(r.contains("Title: Probe Page"), "{}", r);
        assert!(r.contains("hello from console"), "{}", r);
        assert!(r.contains("Uncaught ReferenceError: undefinedFn is not defined"), "{}", r);
        assert!(r.contains("1 uncaught error"), "{}", r);
        // The DOM is captured after the page's timers ran, and scripts/styles are not text.
        assert!(r.contains("loaded late"), "{}", r);
        assert!(!r.contains("color:red"), "{}", r);

        // The server logged the request; stopping it frees the port.
        assert!(reg.view(id, 4000).unwrap().output.contains("GET / HTTP"));
        reg.stop(id, 1000).unwrap();
        let err = capture_page("http://127.0.0.1:8734/", 800, 600, 500).await.err().expect("refused");
        assert!(err.contains("ERR_CONNECTION_REFUSED"), "{}", err);
        assert!(err.contains("dev server"), "{}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_title() {
        assert_eq!(extract_title("<html><head><TITLE>\n  My  App </TITLE></head>").as_deref(), Some("My App"));
        assert_eq!(extract_title("<html><head></head><body>x</body>"), None);
        assert_eq!(extract_title("<title></title>"), None);
    }
}
