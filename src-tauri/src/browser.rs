//! Headless-browser page capture for the `capture_page` tool.
//!
//! Loads a URL in a Chromium-based browser that is already installed (Chrome, Chromium, Brave
//! or Edge) with `--headless`, so nothing appears on screen and no window is focused, and
//! returns three things from one run: a screenshot, everything the page wrote to the console
//! (including uncaught errors), and the rendered DOM as text. Each run uses a throwaway
//! profile directory so it never touches — or attaches to — the user's own running browser.
//!
//! The caller validates the starting URL (`tools::resolve_page_target`), but a page can
//! redirect or navigate anywhere and load sub-resources from any host. So every request the
//! browser makes goes through a small in-process proxy (`GuardProxy`) that resolves the host
//! itself and refuses private-network, link-local (cloud metadata) and other internal
//! addresses — loopback stays allowed, since previewing local dev servers is the point. It
//! connects to the exact address it checked, so DNS rebinding can't slip past either.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
    let proxy = GuardProxy::start().await?;
    let result = run_capture(browser_name, &exe, &work, url, width, height, wait_ms, proxy.addr).await;
    let _ = std::fs::remove_dir_all(&work);

    let log = proxy.log();
    // The browser may ask for the same blocked host more than once (retries, favicon).
    let mut blocked: Vec<&String> = Vec::new();
    for entry in &log.blocked {
        if !blocked.contains(&entry) {
            blocked.push(entry);
        }
    }
    let blocked_note = if blocked.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nBlocked requests to private-network or internal addresses (only public hosts and this machine's loopback may be loaded): {}",
            blocked.iter().map(|b| b.as_str()).collect::<Vec<_>>().join("; ")
        )
    };
    // Behind the proxy, a refused connection to the page itself comes back as the proxy's
    // 502 (http) or a tunnel failure (https) — report it the way a direct load would.
    if let Some(target) = url_host_port(url) {
        if log.refused.iter().any(|r| r == &target) {
            return Err(format!(
                "Page failed to load: net::ERR_CONNECTION_REFUSED ({}).{}",
                target, CONNECTION_REFUSED_HINT
            ));
        }
    }
    match result {
        Ok(mut capture) => {
            capture.report.push_str(&blocked_note);
            Ok(capture)
        }
        Err(e) => Err(format!("{}{}", e, blocked_note)),
    }
}

const CONNECTION_REFUSED_HINT: &str = " Nothing is listening at that address — is the dev server running and ready? (process action \"output\" with wait_for shows its log.)";

/// `host:port` of an http(s) URL as the browser will request it through the proxy.
fn url_host_port(url: &str) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    let (rest, default_port) = if let Some(r) = lower.strip_prefix("http://") {
        (r, 80)
    } else if let Some(r) = lower.strip_prefix("https://") {
        (r, 443)
    } else {
        return None;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = split_host_port(authority, default_port)?;
    Some(format!("{}:{}", host, port))
}

/// Split `host[:port]` / `[v6]:port`, lowercasing the host and dropping IPv6 brackets.
fn split_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port,
        };
        (h, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, default_port),
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some((host, port))
}

// ---------------------------------------------------------------------------------------
// Address guard
// ---------------------------------------------------------------------------------------

/// Whether the headless browser may connect to `ip`: public addresses and loopback yes;
/// private networks, link-local (incl. 169.254.169.254 metadata), CGNAT, unspecified,
/// broadcast and multicast no.
fn ip_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_allowed(v4);
            }
            if v6.is_loopback() {
                return true;
            }
            let seg0 = v6.segments()[0];
            let unique_local = (seg0 & 0xfe00) == 0xfc00;
            let link_local = (seg0 & 0xffc0) == 0xfe80;
            !(v6.is_unspecified() || v6.is_multicast() || unique_local || link_local)
        }
    }
}

fn v4_allowed(ip: Ipv4Addr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    let [a, b, _, _] = ip.octets();
    let cgnat = a == 100 && (64..=127).contains(&b);
    !(a == 0
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || cgnat)
}

#[derive(Default, Clone)]
struct ProxyLog {
    /// `host:port (why)` of every request refused because of where it resolved.
    blocked: Vec<String>,
    /// `host:port` of allowed targets whose connection was refused.
    refused: Vec<String>,
}

/// The local HTTP proxy every `capture_page` browser request goes through. Aborted on drop.
struct GuardProxy {
    addr: SocketAddr,
    log: Arc<Mutex<ProxyLog>>,
    task: tokio::task::JoinHandle<()>,
}

impl GuardProxy {
    async fn start() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("could not start the page-capture proxy: {}", e))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let log = Arc::new(Mutex::new(ProxyLog::default()));
        let task_log = log.clone();
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(handle_proxy_conn(sock, task_log.clone()));
            }
        });
        Ok(Self { addr, log, task })
    }

    fn log(&self) -> ProxyLog {
        self.log.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl Drop for GuardProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Resolve `host` and return its addresses, refusing the request if ANY of them is not
/// allowed (so a name can't mix a public and a private record).
async fn resolve_allowed(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("could not resolve: {}", e))?
        .collect();
    if addrs.is_empty() {
        return Err("could not resolve".to_string());
    }
    if let Some(bad) = addrs.iter().find(|a| !ip_allowed(a.ip())) {
        return Err(format!("resolves to the internal address {}", bad.ip()));
    }
    Ok(addrs)
}

async fn respond(client: &mut TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    let _ = client.write_all(msg.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// Serve one proxied connection: a `CONNECT host:port` tunnel (https, websockets) or one
/// absolute-form plain-http request, forwarded with `Connection: close` so every request
/// opens a new connection and is checked on its own.
async fn handle_proxy_conn(mut client: TcpStream, log: Arc<Mutex<ProxyLog>>) {
    const MAX_HEAD: usize = 64 * 1024;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return;
        }
        match client.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let rest = buf[head_end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let (Some(method), Some(target), Some(version)) = (request_line.next(), request_line.next(), request_line.next()) else {
        respond(&mut client, "400 Bad Request", "malformed request").await;
        return;
    };
    let is_connect = method.eq_ignore_ascii_case("CONNECT");

    let (host, port, origin_form) = if is_connect {
        match split_host_port(target, 443) {
            Some((h, p)) => (h, p, String::new()),
            None => return respond(&mut client, "400 Bad Request", "bad CONNECT target").await,
        }
    } else {
        let Some(after) = target.strip_prefix("http://").or_else(|| target.strip_prefix("HTTP://")) else {
            return respond(&mut client, "400 Bad Request", "only http:// requests can be proxied").await;
        };
        let split = after.find(['/', '?', '#']).unwrap_or(after.len());
        let authority = &after[..split];
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let path = &after[split..];
        let path = if path.is_empty() || !path.starts_with('/') { format!("/{}", path) } else { path.to_string() };
        match split_host_port(authority, 80) {
            Some((h, p)) => (h, p, path),
            None => return respond(&mut client, "400 Bad Request", "bad request target").await,
        }
    };
    let label = format!("{}:{}", host, port);

    let addrs = match resolve_allowed(&host, port).await {
        Ok(a) => a,
        Err(why) => {
            log.lock().unwrap_or_else(|p| p.into_inner()).blocked.push(format!("{} ({})", label, why));
            return respond(&mut client, "403 Forbidden", &format!("Muninn blocked {}: it {}", label, why)).await;
        }
    };
    // Connect to the exact addresses that were checked (pins against DNS rebinding), trying
    // each — `localhost` may resolve to ::1 first while a dev server listens on 127.0.0.1.
    let mut upstream = None;
    for addr in &addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await {
            upstream = Some(s);
            break;
        }
    }
    let Some(mut upstream) = upstream else {
        log.lock().unwrap_or_else(|p| p.into_inner()).refused.push(label.clone());
        return respond(&mut client, "502 Bad Gateway", &format!("could not connect to {}", label)).await;
    };

    if is_connect {
        if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
    } else {
        let mut out = format!("{} {} {}\r\n", method, origin_form, version);
        for line in lines {
            let name = line.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
            if matches!(name.as_str(), "proxy-connection" | "connection" | "keep-alive" | "proxy-authorization") {
                continue;
            }
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.push_str("Connection: close\r\n\r\n");
        if upstream.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
    if !rest.is_empty() && upstream.write_all(&rest).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

#[allow(clippy::too_many_arguments)]
async fn run_capture(
    browser_name: &str,
    exe: &Path,
    work: &Path,
    url: &str,
    width: u32,
    height: u32,
    wait_ms: u64,
    proxy: SocketAddr,
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
        // Every request — loopback included (`<-loopback>` removes Chrome's implicit
        // bypass) — goes through the address guard; WebRTC may not open direct UDP.
        .arg(format!("--proxy-server=http://{}", proxy))
        .arg("--proxy-bypass-list=<-loopback>")
        .arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp")
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
            CONNECTION_REFUSED_HINT
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
        let mut scanned = 0u64;
        loop {
            let (found, running) = reg.log_contains("e2e", id, "serving http", &mut scanned).unwrap();
            if found {
                break;
            }
            assert!(running, "server exited early: {}", reg.view("e2e", id, 2000).unwrap().output);
            assert!(std::time::Instant::now() < deadline, "server never became ready: {}", reg.view("e2e", id, 2000).unwrap().output);
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

        // A page that navigates to the metadata address is stopped at the guard.
        std::fs::write(
            dir.join("redir.html"),
            r#"<!doctype html><html><body><script>location.href = "http://169.254.169.254/latest/meta-data/";</script></body></html>"#,
        )
        .unwrap();
        let capture = capture_page("http://127.0.0.1:8734/redir.html", 800, 600, 2000).await.unwrap();
        assert!(capture.report.contains("Blocked requests to private-network"), "{}", capture.report);
        assert_eq!(capture.report.matches("169.254.169.254:80 (").count(), 1, "{}", capture.report);
        assert!(capture.report.contains("169.254.169.254:80"), "{}", capture.report);

        // The server logged the request; stopping it frees the port.
        assert!(reg.view("e2e", id, 4000).unwrap().output.contains("GET / HTTP"));
        reg.stop("e2e", id, 1000).unwrap();
        let err = capture_page("http://127.0.0.1:8734/", 800, 600, 500).await.err().expect("refused");
        assert!(err.contains("ERR_CONNECTION_REFUSED"), "{}", err);
        assert!(err.contains("dev server"), "{}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_public_and_loopback_addresses_are_allowed() {
        for ok in ["8.8.8.8", "1.1.1.1", "127.0.0.1", "127.8.9.10", "::1", "2606:4700::1111", "::ffff:8.8.8.8"] {
            assert!(ip_allowed(ok.parse().unwrap()), "{} should be allowed", ok);
        }
        for bad in [
            "10.0.0.1", "172.16.5.4", "172.31.255.255", "192.168.1.1", "169.254.169.254", "100.64.0.1",
            "0.0.0.0", "255.255.255.255", "224.0.0.1", "fd00::1", "fe80::1", "::", "ff02::1",
            "::ffff:192.168.1.1", "::ffff:169.254.169.254",
        ] {
            assert!(!ip_allowed(bad.parse().unwrap()), "{} should be blocked", bad);
        }
    }

    #[test]
    fn host_port_parsing() {
        assert_eq!(url_host_port("http://localhost:5173/app?x=1").as_deref(), Some("localhost:5173"));
        assert_eq!(url_host_port("https://Example.com/").as_deref(), Some("example.com:443"));
        assert_eq!(url_host_port("http://user@[::1]:8080/").as_deref(), Some("::1:8080"));
        assert_eq!(url_host_port("file:///tmp/x.html"), None);
        assert_eq!(split_host_port("example.com", 443), Some(("example.com".into(), 443)));
        assert_eq!(split_host_port("[::1]:9", 443), Some(("::1".into(), 9)));
        assert_eq!(split_host_port("host:notaport", 80), None);
    }

    async fn proxy_request(proxy: SocketAddr, request: &str) -> String {
        let mut sock = TcpStream::connect(proxy).await.unwrap();
        sock.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), sock.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).to_string()
    }

    /// The guard forwards loopback traffic, but refuses private-network and metadata hosts
    /// — whether asked for directly, via CONNECT, or by a name that resolves there.
    #[tokio::test]
    async fn guard_proxy_forwards_loopback_and_blocks_internal_hosts() {
        // A tiny origin server on loopback that echoes the request line it received.
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = origin.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let first = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string();
                let body = format!("origin saw: {}", first);
                let _ = s
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).as_bytes())
                    .await;
            }
        });

        let proxy = GuardProxy::start().await.unwrap();
        let ok = proxy_request(
            proxy.addr,
            &format!("GET http://127.0.0.1:{}/page?q=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nProxy-Connection: keep-alive\r\n\r\n", origin_addr.port()),
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{}", ok);
        assert!(ok.contains("origin saw: GET /page?q=1 HTTP/1.1"), "{}", ok);

        let metadata = proxy_request(proxy.addr, "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(metadata.starts_with("HTTP/1.1 403"), "{}", metadata);
        let tunnel = proxy_request(proxy.addr, "CONNECT 192.168.1.1:443 HTTP/1.1\r\nHost: 192.168.1.1:443\r\n\r\n").await;
        assert!(tunnel.starts_with("HTTP/1.1 403"), "{}", tunnel);
        let v6 = proxy_request(proxy.addr, "CONNECT [fd00::1]:443 HTTP/1.1\r\n\r\n").await;
        assert!(v6.starts_with("HTTP/1.1 403"), "{}", v6);

        // Nothing listening: a 502 and the target is recorded as refused.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let refused = proxy_request(proxy.addr, &format!("GET http://127.0.0.1:{}/ HTTP/1.1\r\n\r\n", dead)).await;
        assert!(refused.starts_with("HTTP/1.1 502"), "{}", refused);

        let log = proxy.log();
        assert_eq!(log.blocked.len(), 3, "{:?}", log.blocked);
        assert!(log.blocked[0].starts_with("169.254.169.254:80"), "{:?}", log.blocked);
        assert_eq!(log.refused, vec![format!("127.0.0.1:{}", dead)]);
    }

    #[test]
    fn extracts_title() {
        assert_eq!(extract_title("<html><head><TITLE>\n  My  App </TITLE></head>").as_deref(), Some("My App"));
        assert_eq!(extract_title("<html><head></head><body>x</body>"), None);
        assert_eq!(extract_title("<title></title>"), None);
    }
}
