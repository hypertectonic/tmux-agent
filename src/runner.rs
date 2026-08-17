use crate::codex;
use crate::config::RuntimePaths;
use crate::detect::{self, Detection};
use crate::model::{AgentState, DetectionDetails, EvidenceSource, GoalInfo};
use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, ExitStatus, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const RUNNER_PROTOCOL: u32 = 2;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const RUNNER_EXPIRY_MS: u64 = 3_000;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerState {
    pub protocol: u32,
    pub run_id: String,
    pub owner_pid: u32,
    pub child_pid: u32,
    pub process_group: u32,
    pub agent: String,
    pub state: AgentState,
    pub source: EvidenceSource,
    pub cwd: String,
    pub title: String,
    pub outer_terminal: Option<String>,
    pub inner_terminal: Option<String>,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionDetails>,
}

impl RunnerState {
    pub fn as_detection(&self) -> Detection {
        Detection {
            agent: self.agent.clone(),
            state: self.state,
            source: self.source,
            goal: self.goal,
            details: self.detection.clone(),
        }
    }

    fn same_observation(&self, other: &Self) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        left.updated_at_ms = 0;
        right.updated_at_ms = 0;
        left == right
    }
}

#[derive(Debug, Default)]
struct TitleCallbacks {
    title: String,
}

impl vt100::Callbacks for TitleCallbacks {
    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, value: &[u8]) {
        self.title = String::from_utf8_lossy(value).into_owned();
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, value: &[u8]) {
        self.title = String::from_utf8_lossy(value).into_owned();
    }
}

struct TerminalBuffer {
    parser: vt100::Parser<TitleCallbacks>,
}

impl TerminalBuffer {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows.max(2),
                cols.max(2),
                2_000,
                TitleCallbacks::default(),
            ),
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(2), cols.max(2));
    }

    fn evidence(&self) -> (String, String) {
        (
            self.parser.screen().contents(),
            self.parser.callbacks().title.clone(),
        )
    }
}

struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    fn enter() -> Result<Self> {
        let enabled = io::stdin().is_terminal();
        if enabled {
            crossterm::terminal::enable_raw_mode().context("enable terminal raw mode")?;
        }
        Ok(Self { enabled })
    }

    fn suspend(&mut self) -> Result<()> {
        if self.enabled {
            crossterm::terminal::disable_raw_mode()
                .context("restore terminal before suspension")?;
            self.enabled = false;
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if !self.enabled && io::stdin().is_terminal() {
            crossterm::terminal::enable_raw_mode().context("restore raw mode after suspension")?;
            self.enabled = true;
        }
        Ok(())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

#[derive(Default)]
struct JobControl {
    requested: Mutex<bool>,
    changed: Condvar,
}

impl JobControl {
    fn request_and_wait(&self) {
        let mut requested = self
            .requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *requested = true;
        self.changed.notify_all();
        while *requested {
            requested = self
                .changed
                .wait(requested)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn is_requested(&self) -> bool {
        *self
            .requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn complete(&self) {
        let mut requested = self
            .requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *requested = false;
        self.changed.notify_all();
    }
}

struct RunnerFile {
    path: PathBuf,
}

impl RunnerFile {
    fn new(directory: &Path, run_id: &str) -> Self {
        Self {
            path: directory.join(format!("{run_id}.json")),
        }
    }

    fn publish(&self, state: &RunnerState) -> Result<()> {
        write_state(&self.path, state)
    }
}

impl Drop for RunnerFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct ChildGuard {
    child: Box<dyn Child + Send + Sync>,
    running: bool,
    child_pid: Option<u32>,
    process_group: Option<u32>,
}

impl ChildGuard {
    fn new(child: Box<dyn Child + Send + Sync>) -> Self {
        Self {
            child,
            running: true,
            child_pid: None,
            process_group: None,
        }
    }

    fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    fn set_identity(&mut self, child_pid: u32, process_group: u32) {
        self.child_pid = Some(child_pid);
        self.process_group = Some(process_group);
    }

    fn kill_session(&mut self) -> io::Result<()> {
        let mut first_error = None;
        #[cfg(unix)]
        if let (Some(child_pid), Some(process_group)) = (self.child_pid, self.process_group)
            && let Err(error) = signal_pty_session(child_pid, process_group, libc::SIGKILL)
        {
            first_error = Some(error);
        }
        if let Err(error) = self.child.kill()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.running = false;
        }
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.running {
            let _ = self.kill_session();
            let _ = self.child.wait();
        }
    }
}

pub fn run(command: Vec<OsString>, paths: &RuntimePaths) -> Result<i32> {
    let command_description = describe_command(&command);
    if command.is_empty() {
        bail!("run requires a command after --");
    }
    let agent = detect::agent_for_argv(&command).with_context(|| {
        format!("run requires a supported agent command, got {command_description:?}")
    })?;
    let codex_thread_id = agent
        .eq_ignore_ascii_case("codex")
        .then(|| codex::resume_thread_id_from_argv(&command))
        .flatten();

    paths.ensure_dirs()?;
    let owner_pid = std::process::id();
    let run_id = unique_run_id(owner_pid);
    let cwd = std::env::current_dir()
        .context("read current working directory")?
        .to_string_lossy()
        .into_owned();
    let outer_terminal = terminal_name(0);
    let (cols, rows) = terminal_size();
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open agent PTY")?;
    let inner_terminal = pair
        .master
        .tty_name()
        .and_then(|path| path.to_str().map(normalize_terminal));

    let mut builder = CommandBuilder::from_argv(command);
    builder.cwd(&cwd);
    builder.env("TMUX_AGENT_RUN_ID", &run_id);
    let child = pair
        .slave
        .spawn_command(builder)
        .with_context(|| format!("start {command_description}"))?;
    let mut child = ChildGuard::new(child);
    drop(pair.slave);
    let child_pid = child
        .process_id()
        .context("agent PTY did not report a child process ID")?;
    let process_group = pair
        .master
        .process_group_leader()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(child_pid);
    child.set_identity(child_pid, process_group);
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("clone agent PTY reader")?;
    let mut writer = pair.master.take_writer().context("take agent PTY writer")?;

    let mut raw_mode = RawModeGuard::enter()?;
    let job_control_enabled = raw_mode.enabled;
    let terminal_buffer = Arc::new(Mutex::new(TerminalBuffer::new(rows, cols)));
    let output_buffer = terminal_buffer.clone();
    let output_failed = Arc::new(AtomicBool::new(false));
    let output_thread_failed = output_failed.clone();
    let output_thread = thread::spawn(move || {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        let mut bytes = [0_u8; 16 * 1024];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            if stdout.write_all(&bytes[..count]).is_err() || stdout.flush().is_err() {
                output_thread_failed.store(true, Ordering::Relaxed);
                break;
            }
            if let Ok(mut buffer) = output_buffer.lock() {
                buffer.process(&bytes[..count]);
            }
        }
    });

    let job_control = Arc::new(JobControl::default());
    let input_job_control = job_control.clone();
    let input_thread = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut bytes = [0_u8; 8 * 1024];
        while let Ok(count) = stdin.read(&mut bytes) {
            if count == 0
                || forward_input(
                    &bytes[..count],
                    &mut writer,
                    job_control_enabled.then_some(input_job_control.as_ref()),
                )
                .is_err()
            {
                break;
            }
            let _ = writer.flush();
        }
    });

    let runner_file = RunnerFile::new(&paths.runners, &run_id);
    let termination_signal = termination_signal()?;
    let mut last_state = None::<RunnerState>;
    let mut last_publish = Instant::now()
        .checked_sub(HEARTBEAT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut current_size = (cols, rows);
    let mut current_cwd = cwd;
    let mut last_cwd_refresh = Instant::now()
        .checked_sub(HEARTBEAT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut termination_started = None::<Instant>;
    let mut forced_termination = false;

    let exit_status = loop {
        let requested_signal = termination_signal.load(Ordering::Relaxed);
        if termination_started.is_none() && requested_signal != 0 {
            #[cfg(unix)]
            signal_process_group(process_group, requested_signal as libc::c_int)
                .context("forward termination signal to agent process group")?;
            #[cfg(not(unix))]
            child.kill().context("terminate agent process")?;
            termination_started = Some(Instant::now());
        } else if termination_started.is_none() && output_failed.load(Ordering::Relaxed) {
            child
                .kill_session()
                .context("terminate agent after output closed")?;
            termination_started = Some(Instant::now());
            forced_termination = true;
        } else if !forced_termination
            && termination_started
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(1))
        {
            child
                .kill_session()
                .context("force termination of unresponsive agent")?;
            forced_termination = true;
        }
        if let Some(status) = child.try_wait().context("poll agent process")? {
            break status;
        }
        if job_control.is_requested() {
            let result = suspend_for_job_control(&mut raw_mode, process_group);
            job_control.complete();
            result?;
        }

        if last_cwd_refresh.elapsed() >= HEARTBEAT_INTERVAL {
            if let Some(cwd) = process_working_directory(child_pid) {
                current_cwd = cwd;
            }
            last_cwd_refresh = Instant::now();
        }

        let size = terminal_size();
        if size != current_size {
            current_size = size;
            pair.master
                .resize(PtySize {
                    rows: size.1,
                    cols: size.0,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize agent PTY")?;
            if let Ok(mut buffer) = terminal_buffer.lock() {
                buffer.resize(size.1, size.0);
            }
        }

        let state = observed_state(
            &terminal_buffer,
            RunnerIdentity {
                run_id: &run_id,
                owner_pid,
                child_pid,
                process_group,
                agent: &agent,
                cwd: &current_cwd,
                outer_terminal: outer_terminal.as_deref(),
                inner_terminal: inner_terminal.as_deref(),
                codex_thread_id: codex_thread_id.as_deref(),
            },
        )?;
        let changed = last_state
            .as_ref()
            .is_none_or(|previous| !previous.same_observation(&state));
        if changed || last_publish.elapsed() >= HEARTBEAT_INTERVAL {
            runner_file.publish(&state)?;
            last_state = Some(state);
            last_publish = Instant::now();
        }
        thread::sleep(POLL_INTERVAL);
    };

    #[cfg(unix)]
    signal_pty_session(child_pid, process_group, libc::SIGKILL)
        .context("clean up agent PTY session")?;
    drop(pair.master);
    let _ = output_thread.join();
    drop(input_thread);
    drop(runner_file);
    Ok(shell_exit_code(&exit_status))
}

struct RunnerIdentity<'a> {
    run_id: &'a str,
    owner_pid: u32,
    child_pid: u32,
    process_group: u32,
    agent: &'a str,
    cwd: &'a str,
    outer_terminal: Option<&'a str>,
    inner_terminal: Option<&'a str>,
    codex_thread_id: Option<&'a str>,
}

fn observed_state(
    buffer: &Arc<Mutex<TerminalBuffer>>,
    identity: RunnerIdentity<'_>,
) -> Result<RunnerState> {
    let (screen, title) = buffer
        .lock()
        .map_err(|_| anyhow::anyhow!("agent terminal buffer lock is poisoned"))?
        .evidence();
    let detection = detect::detect_agent(identity.agent.to_string(), &title, &screen);
    let title = detect::stable_title(&detection.agent, &title).unwrap_or(title);
    Ok(RunnerState {
        protocol: RUNNER_PROTOCOL,
        run_id: identity.run_id.to_string(),
        owner_pid: identity.owner_pid,
        child_pid: identity.child_pid,
        process_group: identity.process_group,
        agent: detection.agent,
        state: detection.state,
        source: detection.source,
        cwd: identity.cwd.to_string(),
        title,
        outer_terminal: identity.outer_terminal.map(str::to_string),
        inner_terminal: identity.inner_terminal.map(str::to_string),
        updated_at_ms: now_ms(),
        codex_thread_id: identity.codex_thread_id.map(str::to_string),
        goal: detection.goal,
        detection: detection.details,
    })
}

pub fn load_states(
    directory: &Path,
    live_pids: &std::collections::HashSet<u32>,
) -> Vec<RunnerState> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let now = now_ms();
    let mut states = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension() != Some(OsStr::new("json")) {
                return None;
            }
            let state = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RunnerState>(&bytes).ok());
            let valid = state.as_ref().is_some_and(|state| {
                state.protocol == RUNNER_PROTOCOL
                    && live_pids.contains(&state.owner_pid)
                    && live_pids.contains(&state.child_pid)
                    && now.saturating_sub(state.updated_at_ms) <= RUNNER_EXPIRY_MS
            });
            if !valid {
                let _ = fs::remove_file(&path);
            }
            state.filter(|_| valid)
        })
        .collect::<Vec<_>>();
    states.sort_by(|left, right| left.run_id.cmp(&right.run_id));
    states
}

fn describe_command(command: &[OsString]) -> String {
    command
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn unique_run_id(pid: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{pid:x}-{nanos:x}")
}

fn write_state(path: &Path, state: &RunnerState) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("runner path has no parent: {}", path.display()))?;
    let temporary = parent.join(format!(".{}.{}.tmp", state.run_id, state.owner_pid));
    let bytes = serde_json::to_vec(state).context("serialize runner state")?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create runner state {}", temporary.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write runner state {}", temporary.display()))?;
    file.flush()
        .with_context(|| format!("flush runner state {}", temporary.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure runner state {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("publish runner state {}", path.display()))
}

fn forward_input(
    bytes: &[u8],
    writer: &mut dyn Write,
    job_control: Option<&JobControl>,
) -> io::Result<()> {
    let Some(job_control) = job_control else {
        return writer.write_all(bytes);
    };
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != 0x1a {
            continue;
        }
        writer.write_all(&bytes[start..index])?;
        writer.flush()?;
        job_control.request_and_wait();
        start = index + 1;
    }
    writer.write_all(&bytes[start..])
}

#[cfg(unix)]
fn suspend_for_job_control(raw_mode: &mut RawModeGuard, process_group: u32) -> Result<()> {
    raw_mode.suspend()?;
    signal_process_group(process_group, libc::SIGTSTP).context("suspend agent process group")?;
    if unsafe { libc::raise(libc::SIGTSTP) } != 0 {
        return Err(io::Error::last_os_error()).context("suspend tmux-agent runner");
    }
    raw_mode.resume()?;
    signal_process_group(process_group, libc::SIGCONT).context("resume agent process group")
}

#[cfg(not(unix))]
fn suspend_for_job_control(raw_mode: &mut RawModeGuard, _: u32) -> Result<()> {
    raw_mode.suspend()?;
    raw_mode.resume()
}

#[cfg(unix)]
fn signal_process_group(process_group: u32, signal: libc::c_int) -> io::Result<()> {
    let process_group = i32::try_from(process_group)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process group exceeds i32"))?;
    let result = unsafe { libc::kill(-process_group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

#[cfg(unix)]
fn signal_pty_session(session_id: u32, process_group: u32, signal: libc::c_int) -> io::Result<()> {
    let mut first_error = signal_process_group(process_group, signal).err();
    for pid in process_session_members(session_id) {
        let result = unsafe { libc::kill(pid, signal) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) && first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(unix)]
fn process_session_members(session_id: u32) -> Vec<libc::pid_t> {
    let Ok(session_id) = libc::pid_t::try_from(session_id) else {
        return Vec::new();
    };
    let Ok(output) = ProcessCommand::new("ps")
        .args(["-axww", "-o", "pid="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let own_pid = libc::pid_t::try_from(std::process::id()).ok();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|value| value.parse::<libc::pid_t>().ok())
        .filter(|pid| Some(*pid) != own_pid)
        .filter(|pid| unsafe { libc::getsid(*pid) } == session_id)
        .collect()
}

fn process_working_directory(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = ProcessCommand::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_lsof_working_directory(&output.stdout)
    }
}

#[cfg(not(target_os = "linux"))]
fn parse_lsof_working_directory(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .find_map(|line| line.strip_prefix('n').map(str::to_string))
}

fn shell_exit_code(status: &ExitStatus) -> i32 {
    if let Some(signal) = status.signal()
        && let Some(number) = signal_number(signal)
    {
        return 128 + number;
    }
    status.exit_code().min(i32::MAX as u32) as i32
}

#[cfg(unix)]
fn signal_number(description: &str) -> Option<i32> {
    let description = description.to_ascii_lowercase();
    let description_label = signal_label(&description);
    let signals = [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGILL,
        libc::SIGTRAP,
        libc::SIGABRT,
        libc::SIGFPE,
        libc::SIGKILL,
        libc::SIGBUS,
        libc::SIGSEGV,
        libc::SIGSYS,
        libc::SIGPIPE,
        libc::SIGALRM,
        libc::SIGTERM,
        libc::SIGURG,
        libc::SIGSTOP,
        libc::SIGTSTP,
        libc::SIGCONT,
        libc::SIGCHLD,
        libc::SIGTTIN,
        libc::SIGTTOU,
        libc::SIGIO,
        libc::SIGXCPU,
        libc::SIGXFSZ,
        libc::SIGVTALRM,
        libc::SIGPROF,
        libc::SIGWINCH,
        libc::SIGUSR1,
        libc::SIGUSR2,
    ];
    let exact = signals.into_iter().find(|signal| {
        let value = unsafe { libc::strsignal(*signal) };
        if value.is_null() {
            return false;
        }
        let value = unsafe { std::ffi::CStr::from_ptr(value) }
            .to_string_lossy()
            .to_ascii_lowercase();
        signal_label(&value) == description_label
    });
    exact.or_else(|| {
        description
            .rsplit_once(':')
            .and_then(|(_, number)| number.trim().parse::<i32>().ok())
            .filter(|number| signals.contains(number))
    })
}

#[cfg(unix)]
fn signal_label(description: &str) -> &str {
    description
        .rsplit_once(':')
        .filter(|(_, suffix)| suffix.trim().parse::<i32>().is_ok())
        .map_or(description, |(label, _)| label.trim())
}

#[cfg(not(unix))]
fn signal_number(_: &str) -> Option<i32> {
    None
}

fn normalize_terminal(value: &str) -> String {
    value.strip_prefix("/dev/").unwrap_or(value).to_string()
}

fn terminal_size() -> (u16, u16) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    (cols.max(2), rows.max(2))
}

#[cfg(unix)]
fn terminal_name(fd: libc::c_int) -> Option<String> {
    let mut buffer = [0 as libc::c_char; 1_024];
    let status = unsafe { libc::ttyname_r(fd, buffer.as_mut_ptr(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let value = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(normalize_terminal(&value.to_string_lossy()))
}

#[cfg(not(unix))]
fn terminal_name(_: libc::c_int) -> Option<String> {
    None
}

fn termination_signal() -> Result<Arc<AtomicUsize>> {
    let signal_number = Arc::new(AtomicUsize::new(0));
    #[cfg(unix)]
    for signal in [
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGQUIT,
    ] {
        signal_hook::flag::register_usize(signal, signal_number.clone(), signal as usize)
            .with_context(|| format!("register signal handler {signal}"))?;
    }
    Ok(signal_number)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn state(updated_at_ms: u64) -> RunnerState {
        RunnerState {
            protocol: RUNNER_PROTOCOL,
            run_id: "run-1".into(),
            owner_pid: 10,
            child_pid: 11,
            process_group: 11,
            agent: "Codex".into(),
            state: AgentState::Working,
            source: EvidenceSource::Screen,
            cwd: "/work".into(),
            title: "⠸ work".into(),
            outer_terminal: Some("ttys001".into()),
            inner_terminal: Some("ttys002".into()),
            updated_at_ms,
            codex_thread_id: None,
            goal: None,
            detection: None,
        }
    }

    #[test]
    fn terminal_buffer_tracks_screen_and_osc_title() {
        let mut buffer = TerminalBuffer::new(24, 80);
        buffer.process(b"\x1b]2;\xe2\xa0\xb8 project\x07Working (2s \xc2\xb7 esc to interrupt)");
        let (screen, title) = buffer.evidence();
        assert!(screen.contains("Working"));
        assert_eq!(title, "⠸ project");
    }

    #[test]
    fn terminal_buffer_clamps_tiny_dimensions() {
        let mut buffer = TerminalBuffer::new(1, 1);
        buffer.process("xxxx⠸".as_bytes());
        buffer.resize(0, 0);
        buffer.process("more⠸".as_bytes());
        assert!(!buffer.evidence().0.is_empty());
    }

    #[test]
    fn grok_owned_pty_quiet_redraw_is_inferred_idle() {
        let buffer = Arc::new(Mutex::new(TerminalBuffer::new(24, 80)));
        buffer
            .lock()
            .unwrap()
            .process(b"\x1b]2;task title - grok\x07");

        let state = observed_state(
            &buffer,
            RunnerIdentity {
                run_id: "run-grok",
                owner_pid: 10,
                child_pid: 11,
                process_group: 11,
                agent: "Grok",
                cwd: "/work",
                outer_terminal: Some("ttys001"),
                inner_terminal: Some("ttys002"),
                codex_thread_id: None,
            },
        )
        .unwrap();

        assert_eq!(state.state, AgentState::Idle);
        assert_eq!(state.source, EvidenceSource::Process);
        assert!(
            state
                .detection
                .as_ref()
                .is_some_and(|details| details.inferred)
        );
    }

    #[test]
    fn omp_owned_pty_publishes_one_stable_title_across_spinner_frames() {
        let observe = |frame: &str| {
            let mut terminal = TerminalBuffer::new(24, 80);
            terminal.process(format!("\x1b]2;π {frame} local-bench\x07").as_bytes());
            observed_state(
                &Arc::new(Mutex::new(terminal)),
                RunnerIdentity {
                    run_id: "run-omp",
                    owner_pid: 10,
                    child_pid: 11,
                    process_group: 11,
                    agent: "OMP",
                    cwd: "/work/local-bench",
                    outer_terminal: Some("ttys001"),
                    inner_terminal: Some("ttys002"),
                    codex_thread_id: None,
                },
            )
            .unwrap()
        };

        let first = observe("⠋");
        let second = observe("⠸");
        assert_eq!(first.title, "local-bench");
        assert_eq!(second.title, "local-bench");
        assert!(first.same_observation(&second));
    }

    #[test]
    fn state_file_contains_derived_state_but_no_screen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("run-1.json");
        let mut state = state(now_ms());
        state.goal = Some(GoalInfo {
            state: crate::model::GoalState::Pursuing,
            elapsed_seconds: 1_122,
            achievement_pending: false,
            achievement_observed_at_ms: 0,
        });
        state.detection = Some(DetectionDetails {
            engine: "provider".into(),
            detector: Some("Codex".into()),
            observed_state: AgentState::Working,
            signal: Some("working_status_row".into()),
            scope: Some("bottom_lines".into()),
            definitive: true,
            inferred: false,
            preserve_previous: false,
            transition: None,
        });
        write_state(&path, &state).unwrap();
        let json = fs::read_to_string(path).unwrap();
        assert!(json.contains("\"state\":\"working\""));
        assert!(json.contains("\"goal\":{\"state\":\"pursuing\",\"elapsed_seconds\":1122}"));
        assert!(json.contains("\"signal\":\"working_status_row\""));
        assert!(!json.contains("\"screen\":"));
        assert!(!json.contains("\"command\":"));
        assert!(!json.contains("\"prompt\":"));
        assert!(!json.contains("\"content\":"));
        assert!(!json.contains("Pursuing goal"));
    }

    #[test]
    fn loader_requires_live_owner_and_child() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("run-1.json");
        write_state(&path, &state(now_ms())).unwrap();
        let live = [10_u32, 11_u32].into_iter().collect();
        assert_eq!(load_states(directory.path(), &live).len(), 1);

        let only_owner = [10_u32].into_iter().collect();
        assert!(load_states(directory.path(), &only_owner).is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn ctrl_z_requests_job_control_without_reaching_the_child() {
        let job_control = Arc::new(JobControl::default());
        let thread_control = job_control.clone();
        let worker = thread::spawn(move || {
            let mut output = Vec::new();
            forward_input(
                b"before\x1aafter",
                &mut output,
                Some(thread_control.as_ref()),
            )
            .unwrap();
            output
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !job_control.is_requested() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(job_control.is_requested());
        job_control.complete();
        assert_eq!(worker.join().unwrap(), b"beforeafter");
    }

    #[test]
    fn ctrl_z_from_noninteractive_input_is_forwarded() {
        let mut output = Vec::new();
        forward_input(b"before\x1aafter", &mut output, None).unwrap();
        assert_eq!(output, b"before\x1aafter");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn parses_runner_working_directory() {
        assert_eq!(
            parse_lsof_working_directory(b"p123\nfcwd\nn/tmp/project with spaces\n").as_deref(),
            Some("/tmp/project with spaces")
        );
    }

    #[cfg(unix)]
    #[test]
    fn signal_exit_uses_shell_convention() {
        assert_eq!(
            shell_exit_code(&ExitStatus::with_signal("Interrupt")),
            128 + libc::SIGINT
        );
        assert_eq!(
            shell_exit_code(&ExitStatus::with_signal("Terminated: 15")),
            143
        );
        let usr1 = unsafe { libc::strsignal(libc::SIGUSR1) };
        let usr1 = unsafe { std::ffi::CStr::from_ptr(usr1) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            shell_exit_code(&ExitStatus::with_signal(&usr1)),
            128 + libc::SIGUSR1
        );
        assert_eq!(shell_exit_code(&ExitStatus::with_exit_code(7)), 7);
    }
}
