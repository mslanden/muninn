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
pub(crate) const OUTPUT_CAP_CHARS: usize = 12_000;
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
/// SIGKILL whatever is left. Always reaps the child.
///
/// The leader is reaped only at the very end. Until then it holds its pid — even as a zombie —
/// so the process-group id can't be handed to anyone else and every group signal here can only
/// reach our own processes. (Reaping first and then signalling the group, as this used to,
/// could hit an unrelated group that was given the freed id in between.)
fn terminate(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    // Already exited (and now reaped): its pid may be reused, so never signal it again.
    // Anything it deliberately left running in the background is left alone.
    if let Ok(Some(status)) = child.try_wait() {
        return Some(status);
    }
    let pid = child.id();
    signal_group(pid, "TERM");
    let start = Instant::now();
    // `group_alive` turns false once only the (zombie) leader is left — zombies can't be
    // signalled — so this ends as soon as everything has actually exited.
    while group_alive(pid) && start.elapsed() < grace {
        std::thread::sleep(Duration::from_millis(50));
    }
    if group_alive(pid) {
        signal_group(pid, "KILL");
    }
    // In case the leader itself is somehow unreachable through the group signal; harmless on
    // a zombie, and its pid is still ours.
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
pub(crate) fn cap_middle(text: &str, cap: usize) -> String {
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

/// Search a log for `needle` (case-insensitive) from byte `*scanned` to the end, in bounded
/// chunks, advancing `*scanned`. Each chunk starts a little before the previous end so a
/// match straddling two chunks (or two polls) is still found.
fn scan_for(path: &Path, needle: &str, scanned: &mut u64) -> bool {
    const CHUNK: u64 = 1024 * 1024;
    let Ok(mut file) = File::open(path) else { return false };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let needle = needle.to_lowercase();
    // Lowercasing can change byte lengths, so overlap generously.
    let overlap = (needle.len() as u64).saturating_mul(4);
    while *scanned < len {
        let start = scanned.saturating_sub(overlap);
        let end = (*scanned + CHUNK).min(len);
        let mut buf = vec![0u8; (end - start) as usize];
        if file.seek(SeekFrom::Start(start)).is_err() || file.read_exact(&mut buf).is_err() {
            return false;
        }
        *scanned = end;
        if String::from_utf8_lossy(&buf).to_lowercase().contains(&needle) {
            return true;
        }
    }
    false
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
    /// Unix time (s) at which the leader was last seen alive (or not yet reaped). Until then
    /// the pid — and so the process-group id — was reserved by us, so any process that was
    /// already running at that moment and is in this group now is ours. 0 = unknown (an
    /// older ledger), in which case a leaderless group is never touched.
    #[serde(default)]
    confirmed_at: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse `ps -o etime` (`[[dd-]hh:]mm:ss`) into seconds.
fn parse_etime(s: &str) -> Option<u64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse::<u64>().ok()).collect::<Option<_>>()?;
    let (h, m, sec) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
}

/// Approximate start times (unix s) of every process currently in process group `pgid`.
fn group_member_starts(pgid: u32) -> Vec<u64> {
    let Ok(out) = Command::new("/bin/ps").args(["-A", "-o", "pid=,pgid=,etime="]).stderr(Stdio::null()).output() else {
        return Vec::new();
    };
    let now = unix_now();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let _pid = cols.next()?;
            let group: u32 = cols.next()?.parse().ok()?;
            let elapsed = parse_etime(cols.next()?)?;
            (group == pgid).then(|| now.saturating_sub(elapsed))
        })
        .collect()
}

/// For a ledger entry whose leader is gone: whether the process group with its id is still
/// ours. A pid (and so a pgid) is never reused while it is reserved, and it was reserved by
/// us at `confirmed_at`; an unrelated group that later got the same id consists only of
/// processes started after that. So the group is ours iff a member predates `confirmed_at`
/// (with 2s of slack for `etime`'s one-second resolution).
fn leaderless_group_is_ours(entry: &LedgerEntry) -> bool {
    entry.confirmed_at > 0
        && group_member_starts(entry.pid).iter().any(|&start| start + 2 <= entry.confirmed_at)
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
/// - leader gone but the group still has members → ours only if one of them was already
///   running when we last saw our leader alive (`leaderless_group_is_ours`). Otherwise our
///   group died after that and the id now belongs to someone else's group → leave it alone.
pub fn reap_orphans(path: &Path) -> Vec<String> {
    let entries = read_ledger(path);
    let mut stopped = Vec::new();
    for entry in &entries {
        let ours = match process_start_time(entry.pid) {
            Some(started) => started == entry.started,
            None => group_alive(entry.pid) && leaderless_group_is_ours(entry),
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
    /// Every process is refreshed first, so `confirmed_at` is only stamped on leaders that
    /// really are still alive (or unreaped) right now.
    fn sync_ledger(&self, inner: &mut Inner) {
        let Some(path) = &self.ledger else { return };
        let now = unix_now();
        for p in inner.procs.values_mut() {
            p.refresh();
        }
        let mut entries: Vec<LedgerEntry> = inner
            .procs
            .values()
            .filter(|p| p.exited.is_none())
            .map(|p| LedgerEntry {
                pid: p.child.id(),
                started: p.started_at.clone(),
                command: p.command.clone(),
                confirmed_at: now,
            })
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
        self.sync_ledger(&mut inner);
        Ok(id)
    }

    /// Current status and the tail of the log. `requester` is the asking chat's session id —
    /// only the chat that started a process (or its subagents) may read or stop it.
    pub fn view(&self, requester: &str, id: u32, tail_chars: usize) -> Result<ProcessView, String> {
        let mut inner = self.lock();
        let p = owned_mut(&mut inner, requester, id)?;
        p.refresh();
        Ok(ProcessView {
            status: p.status_text(),
            running: p.exited.is_none(),
            output: read_log_tail(&p.log, tail_chars),
        })
    }

    /// Whether `needle` (case-insensitive) has appeared in the log, and whether the process is
    /// still running. `scanned` is the caller's position in the log: start it at 0 and pass the
    /// same variable on every poll, so each call reads only what was written since (plus a
    /// small overlap for a match split across polls). Errors when the id is unknown.
    pub fn log_contains(&self, requester: &str, id: u32, needle: &str, scanned: &mut u64) -> Result<(bool, bool), String> {
        let (log, running) = {
            let mut inner = self.lock();
            let p = owned_mut(&mut inner, requester, id)?;
            p.refresh();
            (p.log.clone(), p.exited.is_none())
        };
        Ok((scan_for(&log, needle, scanned), running))
    }

    /// Stop a process (and everything it started) and forget it. Returns its final output.
    pub fn stop(&self, requester: &str, id: u32, tail_chars: usize) -> Result<String, String> {
        let mut p = {
            let mut inner = self.lock();
            owned_mut(&mut inner, requester, id)?;
            inner.procs.remove(&id).ok_or_else(|| no_such(id))?
        };
        p.refresh();
        let was_running = p.exited.is_none();
        if was_running {
            terminate(&mut p.child, Duration::from_millis(2000));
        }
        // Only after it is really gone: a crash between the two must leave it in the ledger.
        self.sync_ledger(&mut self.lock());
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
                    if may_manage(&p.session_id, current_session) { "" } else { " [started by another chat — only it can read or stop this]" }
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
        self.sync_ledger(&mut self.lock());
    }

    /// Stop everything (app quit).
    pub fn kill_all(&self) {
        let removed: Vec<BgProcess> = self.lock().procs.drain().map(|(_, p)| p).collect();
        drop(removed);
        self.sync_ledger(&mut self.lock());
    }
}

/// Whether the chat `requester` may manage a process started by `owner`: the same chat, or a
/// subagent of it (`<owner>::sub-<n>`). Other chats and scheduled tasks may not — a task or a
/// prompt-injected page must not be able to read or kill the user's dev server.
fn may_manage(owner: &str, requester: &str) -> bool {
    owner == requester || requester.split_once("::sub-").map_or(false, |(parent, _)| parent == owner)
}

fn owned_mut<'a>(inner: &'a mut Inner, requester: &str, id: u32) -> Result<&'a mut BgProcess, String> {
    let p = inner.procs.get_mut(&id).ok_or_else(|| no_such(id))?;
    if !may_manage(&p.session_id, requester) {
        return Err(format!(
            "background process {} was started by another chat; only that chat can read or stop it (deleting that chat also stops it)",
            id
        ));
    }
    Ok(p)
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
        let mut scanned = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (found, running) = reg.log_contains("s1", id, "READY ON PORT", &mut scanned).unwrap();
            if found {
                assert!(running);
                break;
            }
            assert!(Instant::now() < deadline, "never saw readiness line");
            std::thread::sleep(Duration::from_millis(50));
        }

        let view = reg.view("s1", id, 4000).unwrap();
        assert!(view.running);
        assert!(view.output.contains("ready on port 1234"));
        assert!(reg.list("s1").contains("running for"));
        assert!(reg.list("other").contains("[started by another chat"));

        let stopped = reg.stop("s1", id, 4000).unwrap();
        assert!(stopped.starts_with(&format!("Stopped process {}", id)), "got: {}", stopped);
        assert!(reg.view("s1", id, 100).is_err());
        assert_eq!(reg.list("s1"), "No background processes.");
    }

    #[test]
    fn background_exit_is_reported() {
        let reg = ProcessRegistry::default();
        let id = reg.start("s1", "echo boom; exit 7", &tmp()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let view = reg.view("s1", id, 4000).unwrap();
            if !view.running {
                assert!(view.status.contains("exit code 7"), "got: {}", view.status);
                assert!(view.output.contains("boom"));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(reg.stop("s1", id, 100).unwrap().contains("had already exited"));
    }

    #[test]
    fn kill_session_only_stops_that_sessions_processes() {
        let reg = ProcessRegistry::default();
        let a = reg.start("a", "sleep 600", &tmp()).unwrap();
        let b = reg.start("b", "sleep 600", &tmp()).unwrap();
        reg.kill_session("a");
        assert!(reg.view("a", a, 10).is_err());
        assert!(reg.view("b", b, 10).unwrap().running);
        reg.kill_all();
        assert!(reg.view("b", b, 10).is_err());
    }

    #[test]
    fn only_the_owning_chat_and_its_subagents_can_read_or_stop_a_process() {
        let reg = ProcessRegistry::default();
        let id = reg.start("chat-a", "sleep 600", &tmp()).unwrap();
        for other in ["chat-b", "task-1", "chat-b::sub-1", "chat-a-lookalike"] {
            assert!(reg.view(other, id, 10).err().unwrap().contains("another chat"), "{}", other);
            assert!(reg.log_contains(other, id, "x", &mut 0).is_err(), "{}", other);
            assert!(reg.stop(other, id, 10).is_err(), "{}", other);
        }
        // Still running after all those refused attempts.
        assert!(reg.view("chat-a", id, 10).unwrap().running);
        assert!(reg.view("chat-a::sub-3", id, 10).unwrap().running);
        assert!(reg.stop("chat-a", id, 10).unwrap().starts_with("Stopped process"));
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

        reg.stop("s", a, 100).unwrap();
        assert_eq!(read_ledger(&path).iter().map(|e| e.command.as_str()).collect::<Vec<_>>(), vec!["sleep 601"]);
        reg.kill_session("s");
        assert!(read_ledger(&path).is_empty());
        assert!(reg.view("s", b, 10).is_err());
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

        // Confirmed alive after the child started (as a sync would have recorded it).
        std::thread::sleep(Duration::from_millis(2100));
        let confirmed_at = unix_now() + 1;
        write_ledger(&path, &[LedgerEntry { pid: pgid, started, command: "wrapper".into(), confirmed_at }]);
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
        write_ledger(&path, &[LedgerEntry { pid, started: "Thu Jan  1 00:00:00 1970".into(), command: "old".into(), confirmed_at: 0 }]);

        assert!(reap_orphans(&path).is_empty());
        std::thread::sleep(Duration::from_millis(300));
        assert!(process_start_time(pid).is_some(), "an unrelated process was killed");
        assert!(read_ledger(&path).is_empty(), "stale entry should be dropped");

        // Entries for processes that are simply gone are dropped quietly too.
        terminate(&mut bystander, Duration::from_millis(500));
        write_ledger(&path, &[LedgerEntry { pid, started: "x".into(), command: "gone".into(), confirmed_at: unix_now() }]);
        assert!(reap_orphans(&path).is_empty());
        // A missing or corrupt ledger is not an error.
        let _ = fs::remove_file(&path);
        assert!(reap_orphans(&path).is_empty());
        fs::write(&path, b"not json").unwrap();
        assert!(reap_orphans(&path).is_empty());
        let _ = fs::remove_file(&path);
    }

    /// Regression: an entry left behind for a process that already exited must not kill an
    /// unrelated process group that later got the same id (leader gone, members alive, all
    /// started after we last saw our own leader).
    #[test]
    fn a_recycled_leaderless_group_is_never_killed() {
        let path = ledger_path("recycled");
        let pid_file = tmp().join(format!("muninn-ledger-recycled-{}.pid", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        // "Our" leader was last confirmed alive a minute before this group existed.
        let confirmed_at = unix_now() - 60;
        let mut wrapper = spawn_detached(&format!("sleep 600 & echo $! > '{}'", pid_file.display()));
        let pgid = wrapper.id();
        wrapper.wait().unwrap();
        assert!(wait_until(|| pid_file.exists() && !fs::read_to_string(&pid_file).unwrap_or_default().trim().is_empty()));
        let child_pid: u32 = fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(group_alive(pgid));

        write_ledger(&path, &[LedgerEntry { pid: pgid, started: "old".into(), command: "stale".into(), confirmed_at }]);
        assert!(reap_orphans(&path).is_empty());
        std::thread::sleep(Duration::from_millis(300));
        assert!(process_start_time(child_pid).is_some(), "an unrelated process group was killed");
        // A ledger from before `confirmed_at` existed never kills a leaderless group either.
        fs::write(&path, format!(r#"[{{"pid":{},"started":"old","command":"legacy"}}]"#, pgid)).unwrap();
        assert!(reap_orphans(&path).is_empty());
        assert!(process_start_time(child_pid).is_some());

        signal_group(pgid, "KILL");
        let _ = fs::remove_file(&pid_file);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn log_scan_is_incremental_and_reaches_past_8mb() {
        let path = tmp().join(format!("muninn-scan-test-{}.log", std::process::id()));
        // A readiness line after 9 MB of output (the old scan stopped reading at 8 MB).
        let mut text = "x".repeat(9 * 1024 * 1024);
        text.push_str("\nServer READY on port 3000\n");
        fs::write(&path, &text).unwrap();
        let mut scanned = 0;
        assert!(scan_for(&path, "ready on port", &mut scanned));

        // A match split across two polls is found, and earlier output isn't re-read.
        fs::write(&path, "starting...\nLis").unwrap();
        let mut scanned = 0;
        assert!(!scan_for(&path, "listening", &mut scanned));
        assert_eq!(scanned, "starting...\nLis".len() as u64);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, b"tening on :8080\n").unwrap();
        assert!(scan_for(&path, "LISTENING", &mut scanned));
        // Nothing new: nothing read, no match.
        let before = scanned;
        assert!(!scan_for(&path, "nope", &mut scanned));
        assert_eq!(scanned, before);
        let _ = fs::remove_file(&path);
    }

    /// Children that ignore SIGTERM are SIGKILLed with the group, and a group that exits on
    /// SIGTERM isn't held for the whole grace period.
    #[test]
    fn terminate_kills_stubborn_children_and_returns_early_when_all_exit() {
        let pid_file = tmp().join(format!("muninn-terminate-test-{}.pid", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        let mut stubborn = spawn_detached(&format!(
            "/bin/sh -c 'trap \"\" TERM; echo $$ > \"{}\"; while :; do sleep 1; done' & wait",
            pid_file.display()
        ));
        assert!(wait_until(|| fs::read_to_string(&pid_file).map_or(false, |t| !t.trim().is_empty())));
        let child_pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
        let pgid = stubborn.id();
        terminate(&mut stubborn, Duration::from_millis(500));
        assert!(wait_until(|| !pid_alive(&child_pid)), "TERM-ignoring child {} survived", child_pid);
        assert!(!group_alive(pgid));
        let _ = fs::remove_file(&pid_file);

        let mut polite = spawn_detached("sleep 600");
        let started = Instant::now();
        terminate(&mut polite, Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(3), "waited {:?}", started.elapsed());
    }

    #[test]
    fn etime_parses() {
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("12:34"), Some(754));
        assert_eq!(parse_etime("01:00:00"), Some(3600));
        assert_eq!(parse_etime("2-03:04:05"), Some(2 * 86_400 + 3 * 3600 + 4 * 60 + 5));
        assert_eq!(parse_etime("bogus"), None);
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
