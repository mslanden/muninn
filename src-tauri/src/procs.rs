//! Shell processes for the agent: foreground commands with a timeout, and long-running
//! background processes (dev servers, watchers) the agent can start, read and stop.
//!
//! Every process is spawned through `/bin/zsh -lc` in its **own process group**, with stdout
//! and stderr appended to one log file. The group is what makes kills reliable — killing only
//! the zsh wrapper would orphan the `node`/`cargo` child that is actually holding the port —
//! and the log file (rather than pipes) means a chatty child can never block on a full pipe
//! and nothing needs a reader thread.
//!
//! Callers are responsible for the dangerous-command check and for resolving `cwd` inside the
//! workspace before calling in here; this module only runs what it is given.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default / maximum time a foreground `run_command` may take before it is killed.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const MAX_TIMEOUT_SECS: u64 = 1800;
/// Most background processes allowed to be alive at once.
pub const MAX_RUNNING: usize = 8;
/// Exited processes kept around (so their final output can still be read) before pruning.
const MAX_EXITED_KEPT: usize = 12;
/// Model-facing cap on command output.
const OUTPUT_CAP_CHARS: usize = 12_000;
/// Most log bytes ever read back from disk for one result.
const LOG_READ_CAP_BYTES: u64 = 256 * 1024;

static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

fn new_log_path() -> PathBuf {
    let seq = LOG_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("muninn-proc-{}-{}-{}.log", std::process::id(), nanos, seq))
}

/// Spawn `command` via a login zsh in `cwd`, in its own process group, logging to `log`.
fn spawn_shell(command: &str, cwd: &Path, log: &Path) -> Result<Child, String> {
    let out = File::create(log).map_err(|e| format!("failed to create log file: {}", e))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new("/bin/zsh");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| e.to_string())
}

/// Send `signal` (e.g. "TERM", "KILL") to the whole process group led by `pid`.
fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .arg(format!("-{}", signal))
        .arg("--")
        .arg(format!("-{}", pid))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Stop a child and everything it started: SIGTERM the group, give it `grace` to exit, then
/// SIGKILL. Always reaps the child.
fn terminate(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    // Already exited (and now reaped): its pid may be reused, so never signal it again.
    // Anything it deliberately left running in the background is left alone.
    if let Ok(Some(status)) = child.try_wait() {
        return Some(status);
    }
    let pid = child.id();
    signal_group(pid, "TERM");
    let start = Instant::now();
    while start.elapsed() < grace {
        if let Ok(Some(status)) = child.try_wait() {
            signal_group(pid, "KILL");
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    signal_group(pid, "KILL");
    let _ = child.kill();
    child.wait().ok()
}

/// Read a log file for the model: the whole thing when small, otherwise the head and the tail
/// with the middle elided (build and test failures are usually at the end, the command echo
/// and first error at the start).
fn read_log(path: &Path, cap_chars: usize) -> String {
    let Ok(mut file) = File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    if len > LOG_READ_CAP_BYTES {
        // Too big to load whole: head chunk + tail chunk.
        let half = LOG_READ_CAP_BYTES / 2;
        let mut head = vec![0u8; half as usize];
        let n = file.read(&mut head).unwrap_or(0);
        head.truncate(n);
        let mut tail = Vec::new();
        if file.seek(SeekFrom::Start(len - half)).is_ok() {
            let _ = file.read_to_end(&mut tail);
        }
        bytes.extend_from_slice(&head);
        bytes.extend_from_slice(b"\n...[output too large; middle skipped]...\n");
        bytes.extend_from_slice(&tail);
    } else {
        let _ = file.read_to_end(&mut bytes);
    }
    let text = String::from_utf8_lossy(&bytes);
    cap_middle(&text, cap_chars)
}

/// Keep at most `cap` characters of `text`: the first third and the last two thirds.
fn cap_middle(text: &str, cap: usize) -> String {
    let total = text.chars().count();
    if total <= cap {
        return text.to_string();
    }
    let head_n = cap / 3;
    let tail_n = cap - head_n;
    let head: String = text.chars().take(head_n).collect();
    let tail: String = text.chars().skip(total - tail_n).collect();
    format!(
        "{}\n...[{} characters omitted]...\n{}",
        head,
        total - head_n - tail_n,
        tail
    )
}

/// Last `cap` characters of a log.
fn read_log_tail(path: &Path, cap_chars: usize) -> String {
    let Ok(mut file) = File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let want = (cap_chars as u64).saturating_mul(4).min(LOG_READ_CAP_BYTES);
    let truncated = len > want;
    if truncated {
        let _ = file.seek(SeekFrom::Start(len - want));
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let total = text.chars().count();
    if total <= cap_chars && !truncated {
        return text.to_string();
    }
    let tail: String = text.chars().skip(total.saturating_sub(cap_chars)).collect();
    format!("...[earlier output omitted]...\n{}", tail)
}

fn describe_exit(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {}", code),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return format!("killed by signal {}", sig);
                }
            }
            "terminated".to_string()
        }
    }
}

// ---------------------------------------------------------------------------------------
// Foreground commands
// ---------------------------------------------------------------------------------------

/// How a foreground command ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Exited,
    TimedOut,
    Stopped,
}

/// A foreground command in flight. Poll it with `try_finish`; call `kill` to abandon it.
pub struct ShellRun {
    child: Child,
    log: PathBuf,
    started: Instant,
}

impl ShellRun {
    pub fn spawn(command: &str, cwd: &Path) -> Result<Self, String> {
        let log = new_log_path();
        let child = spawn_shell(command, cwd, &log).map_err(|e| {
            let _ = fs::remove_file(&log);
            e
        })?;
        Ok(Self { child, log, started: Instant::now() })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// `Some(text)` once the command has exited (output + a non-zero exit note).
    pub fn try_finish(&mut self) -> Result<Option<String>, String> {
        match self.child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => Ok(Some(self.finish(Outcome::Exited, Some(status), 0))),
            None => Ok(None),
        }
    }

    /// Kill the command (and its children) and return what it printed, with a note saying why.
    pub fn kill(mut self, why: Outcome, timeout_secs: u64) -> String {
        let status = terminate(&mut self.child, Duration::from_millis(1500));
        self.finish(why, status, timeout_secs)
    }

    fn finish(&mut self, outcome: Outcome, status: Option<ExitStatus>, timeout_secs: u64) -> String {
        let mut out = read_log(&self.log, OUTPUT_CAP_CHARS);
        let _ = fs::remove_file(&self.log);
        match outcome {
            Outcome::Exited => {
                // A silent failure is the worst case for verification — always say so.
                if let Some(status) = status.filter(|s| !s.success()) {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&format!("[{}]", describe_exit(&status)));
                }
            }
            Outcome::TimedOut => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "[timed out after {}s and was killed. If this is a long-running process such as a dev server or watcher, start it with background: true instead; if it just needs longer, pass a larger timeout_secs (max {}).]",
                    timeout_secs, MAX_TIMEOUT_SECS
                ));
            }
            Outcome::Stopped => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("[stopped by the user before it finished]");
            }
        }
        out
    }
}

/// Run a command to completion, blocking the calling thread, killing it after `timeout`.
pub fn run_blocking(command: &str, cwd: &Path, timeout: Duration) -> Result<String, String> {
    let mut run = ShellRun::spawn(command, cwd)?;
    loop {
        if let Some(out) = run.try_finish()? {
            return Ok(out);
        }
        if run.elapsed() >= timeout {
            return Ok(run.kill(Outcome::TimedOut, timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// Run a command to completion without blocking the async runtime. `should_stop` is polled
/// while it runs so the chat's Stop button can interrupt a long command.
pub async fn run_async(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    should_stop: impl Fn() -> bool,
) -> Result<String, String> {
    let mut run = ShellRun::spawn(command, cwd)?;
    loop {
        if let Some(out) = run.try_finish()? {
            return Ok(out);
        }
        if should_stop() {
            return Ok(run.kill(Outcome::Stopped, 0));
        }
        if run.elapsed() >= timeout {
            return Ok(run.kill(Outcome::TimedOut, timeout.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

// ---------------------------------------------------------------------------------------
// Background processes
// ---------------------------------------------------------------------------------------

/// One running background process as recorded on disk, so a later launch of the app can
/// find and stop it if this run ends without cleaning up (crash, SIGKILL, power loss).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct LedgerEntry {
    /// Pid of the group leader, which is also the process-group id.
    pid: u32,
    /// The leader's start time as `ps` reports it — tells our process apart from an
    /// unrelated one that was later given the same pid.
    started: String,
    command: String,
}

/// Start time of `pid` as printed by `ps -o lstart=`, or None if there is no such process.
fn process_start_time(pid: u32) -> Option<String> {
    let out = Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Whether any process still belongs to process group `pgid`.
fn group_alive(pgid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg("--")
        .arg(format!("-{}", pgid))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_ledger(path: &Path) -> Vec<LedgerEntry> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_ledger(path: &Path, entries: &[LedgerEntry]) {
    // Write-then-rename so a crash mid-write can't leave a half-written ledger.
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(entries) {
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }
}

/// Stop background processes a previous run of the app left behind, as recorded in the
/// ledger at `path`, and clear it. Returns the commands that were stopped.
///
/// Killing by a stored pid is only safe if it is still *our* process. A pid is never handed
/// to a new process while a process group with that id exists, so:
/// - leader alive with the recorded start time → ours, stop the group;
/// - leader alive with a different start time → the pid was reused, which means our group
///   is completely gone → leave it alone;
/// - leader gone but the group still has members → they are ours (the wrapper exited, its
///   children did not) → stop the group.
pub fn reap_orphans(path: &Path) -> Vec<String> {
    let entries = read_ledger(path);
    let mut stopped = Vec::new();
    for entry in &entries {
        let ours = match process_start_time(entry.pid) {
            Some(started) => started == entry.started,
            None => group_alive(entry.pid),
        };
        if !ours {
            continue;
        }
        signal_group(entry.pid, "TERM");
        let deadline = Instant::now() + Duration::from_millis(1500);
        while group_alive(entry.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if group_alive(entry.pid) {
            signal_group(entry.pid, "KILL");
        }
        stopped.push(entry.command.clone());
    }
    if !entries.is_empty() || path.exists() {
        write_ledger(path, &[]);
    }
    stopped
}

struct BgProcess {
    session_id: String,
    command: String,
    /// Start time of the group leader (see `LedgerEntry::started`).
    started_at: String,
    cwd: String,
    log: PathBuf,
    started: Instant,
    child: Child,
    /// Set once the process is seen to have exited (with how long it had run).
    exited: Option<(ExitStatus, Duration)>,
}

impl BgProcess {
    fn refresh(&mut self) {
        if self.exited.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some((status, self.started.elapsed()));
            }
        }
    }

    fn status_text(&self) -> String {
        match &self.exited {
            Some((status, ran)) => format!("exited ({}) after {}", describe_exit(status), fmt_duration(*ran)),
            None => format!("running for {}", fmt_duration(self.started.elapsed())),
        }
    }
}

impl Drop for BgProcess {
    fn drop(&mut self) {
        if self.exited.is_none() {
            terminate(&mut self.child, Duration::from_millis(300));
        }
        let _ = fs::remove_file(&self.log);
    }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

#[derive(Default)]
struct Inner {
    next_id: u32,
    procs: HashMap<u32, BgProcess>,
}

/// The app-wide set of background processes started by the agent. Lives in `AppState`;
/// dropping it (or calling `kill_all`) stops everything. With a ledger, the running set is
/// also mirrored to disk so the next launch can stop whatever an unclean exit left behind
/// (`reap_orphans`).
#[derive(Default)]
pub struct ProcessRegistry {
    inner: Mutex<Inner>,
    ledger: Option<PathBuf>,
}

/// Snapshot of one process's state after a read.
pub struct ProcessView {
    pub status: String,
    pub running: bool,
    pub output: String,
}

impl ProcessRegistry {
    /// A registry that mirrors its running processes to the ledger file at `path`.
    pub fn with_ledger(path: PathBuf) -> Self {
        Self { inner: Mutex::new(Inner::default()), ledger: Some(path) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Rewrite the ledger from the processes still running. Call after every change.
    fn sync_ledger(&self, inner: &Inner) {
        let Some(path) = &self.ledger else { return };
        let mut entries: Vec<LedgerEntry> = inner
            .procs
            .values()
            .filter(|p| p.exited.is_none())
            .map(|p| LedgerEntry { pid: p.child.id(), started: p.started_at.clone(), command: p.command.clone() })
            .collect();
        entries.sort_by_key(|e| e.pid);
        write_ledger(path, &entries);
    }

    /// Start `command` in the background. Returns its id.
    pub fn start(&self, session_id: &str, command: &str, cwd: &Path) -> Result<u32, String> {
        let mut inner = self.lock();
        for p in inner.procs.values_mut() {
            p.refresh();
        }
        let running = inner.procs.values().filter(|p| p.exited.is_none()).count();
        if running >= MAX_RUNNING {
            return Err(format!(
                "too many background processes are running ({}). Stop one first (process action \"list\" shows them).",
                running
            ));
        }
        // Drop the oldest finished entries so the table can't grow without bound.
        let mut exited: Vec<(u32, Instant)> = inner
            .procs
            .iter()
            .filter(|(_, p)| p.exited.is_some())
            .map(|(id, p)| (*id, p.started))
            .collect();
        if exited.len() >= MAX_EXITED_KEPT {
            exited.sort_by_key(|(_, started)| *started);
            let drop_n = exited.len() + 1 - MAX_EXITED_KEPT;
            for (id, _) in exited.into_iter().take(drop_n) {
                inner.procs.remove(&id);
            }
        }

        let log = new_log_path();
        let child = spawn_shell(command, cwd, &log).map_err(|e| {
            let _ = fs::remove_file(&log);
            e
        })?;
        let started_at = process_start_time(child.id()).unwrap_or_default();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.procs.insert(
            id,
            BgProcess {
                session_id: session_id.to_string(),
                command: command.to_string(),
                started_at,
                cwd: cwd.display().to_string(),
                log,
                started: Instant::now(),
                child,
                exited: None,
            },
        );
        self.sync_ledger(&inner);
        Ok(id)
    }

    /// Current status and the tail of the log.
    pub fn view(&self, id: u32, tail_chars: usize) -> Result<ProcessView, String> {
        let mut inner = self.lock();
        let p = inner.procs.get_mut(&id).ok_or_else(|| no_such(id))?;
        p.refresh();
        Ok(ProcessView {
            status: p.status_text(),
            running: p.exited.is_none(),
            output: read_log_tail(&p.log, tail_chars),
        })
    }

    /// Whether the log contains `needle` (case-insensitive), and whether it is still running.
    /// Errors when the id is unknown.
    pub fn log_contains(&self, id: u32, needle: &str) -> Result<(bool, bool), String> {
        let (log, running) = {
            let mut inner = self.lock();
            let p = inner.procs.get_mut(&id).ok_or_else(|| no_such(id))?;
            p.refresh();
            (p.log.clone(), p.exited.is_none())
        };
        let mut bytes = Vec::new();
        if let Ok(file) = File::open(&log) {
            let _ = file.take(8 * 1024 * 1024).read_to_end(&mut bytes);
        }
        let hay = String::from_utf8_lossy(&bytes).to_lowercase();
        Ok((hay.contains(&needle.to_lowercase()), running))
    }

    /// Stop a process (and everything it started) and forget it. Returns its final output.
    pub fn stop(&self, id: u32, tail_chars: usize) -> Result<String, String> {
        let mut p = self.lock().procs.remove(&id).ok_or_else(|| no_such(id))?;
        p.refresh();
        let was_running = p.exited.is_none();
        if was_running {
            terminate(&mut p.child, Duration::from_millis(2000));
        }
        // Only after it is really gone: a crash between the two must leave it in the ledger.
        self.sync_ledger(&self.lock());
        let output = read_log_tail(&p.log, tail_chars);
        let head = if was_running {
            format!("Stopped process {} ({}).", id, p.command)
        } else {
            format!("Process {} ({}) had already {}; removed it.", id, p.command, p.status_text())
        };
        Ok(if output.trim().is_empty() { head } else { format!("{}\nFinal output:\n{}", head, output) })
    }

    /// One line per process, newest first.
    pub fn list(&self, current_session: &str) -> String {
        let mut inner = self.lock();
        if inner.procs.is_empty() {
            return "No background processes.".to_string();
        }
        let mut ids: Vec<u32> = inner.procs.keys().copied().collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let mut lines = Vec::new();
        for id in ids {
            if let Some(p) = inner.procs.get_mut(&id) {
                p.refresh();
                lines.push(format!(
                    "{}: {} — `{}` (in {}){}",
                    id,
                    p.status_text(),
                    p.command,
                    p.cwd,
                    if p.session_id == current_session { "" } else { " [started by another chat]" }
                ));
            }
        }
        lines.join("\n")
    }

    /// Stop every process started by `session_id` (called when a chat is deleted).
    pub fn kill_session(&self, session_id: &str) {
        let removed: Vec<BgProcess> = {
            let mut inner = self.lock();
            let ids: Vec<u32> = inner
                .procs
                .iter()
                .filter(|(_, p)| p.session_id == session_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter().filter_map(|id| inner.procs.remove(&id)).collect()
        };
        drop(removed); // BgProcess::drop terminates each one, outside the lock.
        self.sync_ledger(&self.lock());
    }

    /// Stop everything (app quit).
    pub fn kill_all(&self) {
        let removed: Vec<BgProcess> = self.lock().procs.drain().map(|(_, p)| p).collect();
        drop(removed);
        self.sync_ledger(&self.lock());
    }
}

fn no_such(id: u32) -> String {
    format!("no background process with id {} (use action \"list\" to see them)", id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir()
    }

    fn pid_alive(pid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn foreground_returns_output_in_order() {
        let out = run_blocking("echo one; echo two 1>&2; echo three", &tmp(), Duration::from_secs(20)).unwrap();
        assert_eq!(out, "one\ntwo\nthree\n");
    }

    #[test]
    fn foreground_reports_nonzero_exit() {
        let out = run_blocking("echo nope; exit 3", &tmp(), Duration::from_secs(20)).unwrap();
        assert!(out.starts_with("nope\n"), "got: {}", out);
        assert!(out.ends_with("[exit code 3]"), "got: {}", out);
    }

    #[test]
    fn foreground_times_out_and_kills_children() {
        // The inner sleep is a grandchild of the zsh wrapper; it must die with the group.
        let started = Instant::now();
        let out = run_blocking(
            "echo started; (sleep 600 & echo $! > muninn-timeout-test.pid; wait)",
            &tmp(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(out.starts_with("started\n"), "got: {}", out);
        assert!(out.contains("timed out after 1s"), "got: {}", out);
        let pid_file = tmp().join("muninn-timeout-test.pid");
        let pid = fs::read_to_string(&pid_file).unwrap_or_default().trim().to_string();
        let _ = fs::remove_file(&pid_file);
        assert!(!pid.is_empty());
        std::thread::sleep(Duration::from_millis(200));
        assert!(!pid_alive(&pid), "grandchild {} survived the timeout kill", pid);
    }

    #[tokio::test]
    async fn async_run_can_be_stopped() {
        let started = Instant::now();
        let out = run_async("echo going; sleep 600", &tmp(), Duration::from_secs(60), || {
            started.elapsed() > Duration::from_millis(400)
        })
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(out.contains("going"));
        assert!(out.ends_with("[stopped by the user before it finished]"), "got: {}", out);
    }

    #[test]
    fn background_lifecycle() {
        let reg = ProcessRegistry::default();
        let id = reg
            .start("s1", "echo ready on port 1234; sleep 600", &tmp())
            .unwrap();

        // Wait for the readiness line.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (found, running) = reg.log_contains(id, "READY ON PORT").unwrap();
            if found {
                assert!(running);
                break;
            }
            assert!(Instant::now() < deadline, "never saw readiness line");
            std::thread::sleep(Duration::from_millis(50));
        }

        let view = reg.view(id, 4000).unwrap();
        assert!(view.running);
        assert!(view.output.contains("ready on port 1234"));
        assert!(reg.list("s1").contains("running for"));
        assert!(reg.list("other").contains("[started by another chat]"));

        let stopped = reg.stop(id, 4000).unwrap();
        assert!(stopped.starts_with(&format!("Stopped process {}", id)), "got: {}", stopped);
        assert!(reg.view(id, 100).is_err());
        assert_eq!(reg.list("s1"), "No background processes.");
    }

    #[test]
    fn background_exit_is_reported() {
        let reg = ProcessRegistry::default();
        let id = reg.start("s1", "echo boom; exit 7", &tmp()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let view = reg.view(id, 4000).unwrap();
            if !view.running {
                assert!(view.status.contains("exit code 7"), "got: {}", view.status);
                assert!(view.output.contains("boom"));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(reg.stop(id, 100).unwrap().contains("had already exited"));
    }

    #[test]
    fn kill_session_only_stops_that_sessions_processes() {
        let reg = ProcessRegistry::default();
        let a = reg.start("a", "sleep 600", &tmp()).unwrap();
        let b = reg.start("b", "sleep 600", &tmp()).unwrap();
        reg.kill_session("a");
        assert!(reg.view(a, 10).is_err());
        assert!(reg.view(b, 10).unwrap().running);
        reg.kill_all();
        assert!(reg.view(b, 10).is_err());
    }

    #[test]
    fn running_process_limit_is_enforced() {
        let reg = ProcessRegistry::default();
        for _ in 0..MAX_RUNNING {
            reg.start("s", "sleep 600", &tmp()).unwrap();
        }
        assert!(reg.start("s", "sleep 600", &tmp()).is_err());
        reg.kill_all();
    }

    fn ledger_path(label: &str) -> PathBuf {
        let p = tmp().join(format!("muninn-ledger-test-{}-{}.json", label, std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    /// Spawn `command` the way the registry does, without a registry owning it.
    fn spawn_detached(command: &str) -> Child {
        let log = new_log_path();
        let child = spawn_shell(command, &tmp(), &log).unwrap();
        let _ = fs::remove_file(&log);
        child
    }

    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        cond()
    }

    #[test]
    fn ledger_tracks_running_processes() {
        let path = ledger_path("tracks");
        let reg = ProcessRegistry::with_ledger(path.clone());
        let a = reg.start("s", "sleep 600", &tmp()).unwrap();
        let b = reg.start("s", "sleep 601", &tmp()).unwrap();
        let entries = read_ledger(&path);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| !e.started.is_empty() && e.pid > 0));
        assert!(entries.iter().any(|e| e.command == "sleep 601"));

        reg.stop(a, 100).unwrap();
        assert_eq!(read_ledger(&path).iter().map(|e| e.command.as_str()).collect::<Vec<_>>(), vec!["sleep 601"]);
        reg.kill_session("s");
        assert!(read_ledger(&path).is_empty());
        assert!(reg.view(b, 10).is_err());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn leftovers_from_an_unclean_exit_are_stopped_at_next_launch() {
        let path = ledger_path("crash");
        let reg = ProcessRegistry::with_ledger(path.clone());
        reg.start("s", "sleep 600", &tmp()).unwrap();
        let pid = read_ledger(&path)[0].pid;
        // Simulate the app dying without cleanup: nothing runs Drop, the process lives on.
        std::mem::forget(reg);
        assert!(group_alive(pid));

        let stopped = reap_orphans(&path);
        assert_eq!(stopped, vec!["sleep 600".to_string()]);
        assert!(wait_until(|| !group_alive(pid)), "leftover process {} survived", pid);
        assert!(read_ledger(&path).is_empty());
        // Nothing left to do the second time.
        assert!(reap_orphans(&path).is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn leftover_children_are_stopped_even_when_their_wrapper_already_exited() {
        let path = ledger_path("orphan-children");
        let pid_file = tmp().join(format!("muninn-ledger-child-{}.pid", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        // The wrapper backgrounds a sleep and exits; the sleep stays in the wrapper's group.
        let mut wrapper = spawn_detached(&format!("sleep 600 & echo $! > '{}'", pid_file.display()));
        let pgid = wrapper.id();
        let started = process_start_time(pgid).unwrap_or_default();
        wrapper.wait().unwrap();
        assert!(wait_until(|| pid_file.exists() && !fs::read_to_string(&pid_file).unwrap_or_default().trim().is_empty()));
        let child_pid: u32 = fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(process_start_time(pgid).is_none(), "wrapper should be gone");
        assert!(process_start_time(child_pid).is_some(), "child should still be running");

        write_ledger(&path, &[LedgerEntry { pid: pgid, started, command: "wrapper".into() }]);
        assert_eq!(reap_orphans(&path), vec!["wrapper".to_string()]);
        assert!(wait_until(|| process_start_time(child_pid).is_none()), "child {} survived", child_pid);
        let _ = fs::remove_file(&pid_file);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_reused_pid_is_never_killed() {
        let path = ledger_path("reuse");
        // A live process whose pid is in the ledger with a DIFFERENT start time stands in
        // for an unrelated process that was later given our old pid.
        let mut bystander = spawn_detached("sleep 600");
        let pid = bystander.id();
        assert!(process_start_time(pid).is_some());
        write_ledger(&path, &[LedgerEntry { pid, started: "Thu Jan  1 00:00:00 1970".into(), command: "old".into() }]);

        assert!(reap_orphans(&path).is_empty());
        std::thread::sleep(Duration::from_millis(300));
        assert!(process_start_time(pid).is_some(), "an unrelated process was killed");
        assert!(read_ledger(&path).is_empty(), "stale entry should be dropped");

        // Entries for processes that are simply gone are dropped quietly too.
        terminate(&mut bystander, Duration::from_millis(500));
        write_ledger(&path, &[LedgerEntry { pid, started: "x".into(), command: "gone".into() }]);
        assert!(reap_orphans(&path).is_empty());
        // A missing or corrupt ledger is not an error.
        let _ = fs::remove_file(&path);
        assert!(reap_orphans(&path).is_empty());
        fs::write(&path, b"not json").unwrap();
        assert!(reap_orphans(&path).is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cap_middle_keeps_head_and_tail() {
        let text: String = (0..1000).map(|i| format!("line{}\n", i)).collect();
        let capped = cap_middle(&text, 300);
        assert!(capped.starts_with("line0\n"));
        assert!(capped.ends_with("line999\n"));
        assert!(capped.contains("characters omitted"));
        assert_eq!(cap_middle("short", 300), "short");
    }
}
