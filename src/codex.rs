use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

mod evidence;
mod ownership;

pub(crate) use evidence::process_rollout_files;
pub(crate) use ownership::{CodexOwnership, ReconciliationFrame};

const DISCOVERY_INTERVAL_MS: u64 = 1_000;
const RECENT_SESSION_DAYS: usize = 7;
const INITIAL_TAIL_BYTES: u64 = 1024 * 1024;
const ACTIVE_STALE_MS: u64 = 30 * 60 * 1_000;

#[derive(Debug, Clone)]
pub struct RolloutMetadata {
    pub path: PathBuf,
    pub thread_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub cwd: String,
    pub started_at_ms: u64,
    pub thread_source: Option<String>,
    pub name: Option<String>,
    pub agent_path: Option<String>,
    pub depth: Option<u32>,
    pub process_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRollout {
    pub thread_id: String,
    pub parent_thread_id: String,
    pub cwd: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub name: Option<String>,
    pub agent_path: Option<String>,
    pub depth: Option<u32>,
    pub process_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRollout {
    pub cwd: String,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
enum Lifecycle {
    Active,
    Finished,
}

#[derive(Debug)]
struct CachedRollout {
    metadata: RolloutMetadata,
    offset: u64,
    aligned: bool,
    lifecycle: Option<(u64, Lifecycle)>,
    last_event_at_ms: u64,
}

impl CachedRollout {
    fn open(metadata: RolloutMetadata) -> Result<Self> {
        let length = fs::metadata(&metadata.path)?.len();
        let offset = length.saturating_sub(INITIAL_TAIL_BYTES);
        let started_at_ms = metadata.started_at_ms;
        let mut cached = Self {
            metadata,
            offset,
            aligned: offset == 0,
            lifecycle: None,
            last_event_at_ms: started_at_ms,
        };
        cached.refresh()?;
        Ok(cached)
    }

    fn refresh(&mut self) -> Result<()> {
        let mut reader = BufReader::new(File::open(&self.metadata.path)?);
        let length = reader.get_ref().metadata()?.len();
        if length < self.offset {
            self.offset = length.saturating_sub(INITIAL_TAIL_BYTES);
            self.aligned = self.offset == 0;
            self.lifecycle = None;
            self.last_event_at_ms = self.metadata.started_at_ms;
        }
        if length == self.offset {
            return Ok(());
        }
        reader.seek(SeekFrom::Start(self.offset))?;
        if !self.aligned {
            reader.seek(SeekFrom::Start(self.offset.saturating_sub(1)))?;
            let mut previous = [0_u8; 1];
            reader.read_exact(&mut previous)?;
            reader.seek(SeekFrom::Start(self.offset))?;
            if previous[0] != b'\n' {
                let mut partial = Vec::new();
                let count = reader.read_until(b'\n', &mut partial)?;
                if !partial.ends_with(b"\n") {
                    return Ok(());
                }
                self.offset = self.offset.saturating_add(count as u64);
            }
            self.aligned = true;
        }
        loop {
            let mut line = String::new();
            let count = reader.read_line(&mut line)?;
            if count == 0 {
                break;
            }
            if !line.ends_with('\n') {
                break;
            }
            self.offset = self.offset.saturating_add(count as u64);
            let Ok(event) = serde_json::from_str::<Value>(line.trim_end()) else {
                continue;
            };
            let event_timestamp = event
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|value| parse_rfc3339_ms(value).ok());
            if let Some(timestamp) = event_timestamp {
                self.last_event_at_ms = self.last_event_at_ms.max(timestamp);
            }
            let Some(kind) = event
                .get("type")
                .and_then(Value::as_str)
                .filter(|kind| *kind == "event_msg")
                .and_then(|_| event["payload"]["type"].as_str())
            else {
                continue;
            };
            let lifecycle = match kind {
                "task_started" => Lifecycle::Active,
                "task_complete" | "turn_aborted" => Lifecycle::Finished,
                _ => continue,
            };
            let timestamp = event_timestamp.unwrap_or(self.metadata.started_at_ms);
            if self
                .lifecycle
                .is_none_or(|(observed_at, _)| timestamp >= observed_at)
            {
                self.lifecycle = Some((timestamp, lifecycle));
            }
        }
        Ok(())
    }

    fn observation(&self, now_ms: u64) -> Option<ThreadRollout> {
        let finished_at_ms = self
            .lifecycle
            .and_then(|(timestamp, lifecycle)| {
                matches!(lifecycle, Lifecycle::Finished).then_some(timestamp)
            })
            .or_else(|| {
                (now_ms.saturating_sub(self.last_event_at_ms) >= ACTIVE_STALE_MS)
                    .then_some(self.last_event_at_ms.saturating_add(ACTIVE_STALE_MS))
            });
        Some(ThreadRollout {
            thread_id: self.metadata.thread_id.clone()?,
            parent_thread_id: self.metadata.parent_thread_id.clone()?,
            cwd: self.metadata.cwd.clone(),
            started_at_ms: self.metadata.started_at_ms,
            finished_at_ms,
            name: self.metadata.name.clone(),
            agent_path: self.metadata.agent_path.clone(),
            depth: self.metadata.depth,
            process_backed: self.metadata.process_backed,
        })
    }
}

#[derive(Debug)]
pub struct ThreadTracker {
    sessions: Option<PathBuf>,
    rollouts: HashMap<PathBuf, CachedRollout>,
    root_rollouts: HashMap<String, RootRollout>,
    ignored: HashSet<PathBuf>,
    failed_lengths: HashMap<PathBuf, u64>,
    expired_lengths: HashMap<PathBuf, u64>,
    last_discovery_ms: Option<u64>,
}

impl ThreadTracker {
    pub fn from_environment() -> Self {
        let sessions = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
            .map(|home| home.join("sessions"));
        Self::new(sessions)
    }

    pub fn new(sessions: Option<PathBuf>) -> Self {
        Self {
            sessions,
            rollouts: HashMap::new(),
            root_rollouts: HashMap::new(),
            ignored: HashSet::new(),
            failed_lengths: HashMap::new(),
            expired_lengths: HashMap::new(),
            last_discovery_ms: None,
        }
    }

    pub fn scan(&mut self, now_ms: u64, retention_ms: u64) -> Vec<ThreadRollout> {
        let discover = self
            .last_discovery_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= DISCOVERY_INTERVAL_MS);
        if discover {
            self.last_discovery_ms = Some(now_ms);
            if let Some(sessions) = &self.sessions
                && let Ok(paths) = collect_recent_rollouts(sessions, now_ms, retention_ms)
            {
                for path in paths {
                    self.discover(path, now_ms, retention_ms);
                }
            }
        }

        for cached in self.rollouts.values_mut() {
            let _ = cached.refresh();
        }
        let observations = self
            .rollouts
            .iter()
            .filter_map(|(path, cached)| {
                cached
                    .observation(now_ms)
                    .map(|thread| (path.clone(), thread))
            })
            .collect::<HashMap<_, _>>();
        let thread_parents = observations
            .values()
            .map(|thread| (thread.thread_id.as_str(), thread.parent_thread_id.as_str()))
            .collect::<HashMap<_, _>>();
        let mut required_ancestors = HashSet::new();
        for thread in observations.values().filter(|thread| {
            thread
                .finished_at_ms
                .is_none_or(|finished| now_ms.saturating_sub(finished) < retention_ms)
        }) {
            let mut parent = thread.parent_thread_id.as_str();
            while let Some(next) = thread_parents.get(parent).copied() {
                if !required_ancestors.insert(parent.to_string()) {
                    break;
                }
                parent = next;
            }
        }
        let mut expired = Vec::new();
        self.rollouts.retain(|path, _| {
            let keep = path.exists()
                && observations.get(path).is_some_and(|thread| {
                    thread
                        .finished_at_ms
                        .is_none_or(|finished| now_ms.saturating_sub(finished) < retention_ms)
                        || required_ancestors.contains(&thread.thread_id)
                });
            if !keep && path.exists() {
                expired.push(path.clone());
            }
            keep
        });
        for path in expired {
            if let Ok(length) = fs::metadata(&path).map(|metadata| metadata.len()) {
                self.expired_lengths.insert(path, length);
            }
        }
        self.rollouts
            .values()
            .filter_map(|cached| cached.observation(now_ms))
            .collect()
    }

    pub fn root_rollouts(&self) -> &HashMap<String, RootRollout> {
        &self.root_rollouts
    }

    pub fn root_thread_id_from_process_rollouts<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> Result<Option<String>> {
        let sessions = self
            .sessions
            .as_deref()
            .context("Codex sessions directory is unavailable")?
            .canonicalize()
            .context("resolve Codex sessions directory")?;
        let mut thread_ids = HashSet::new();
        for path in paths {
            let path = path
                .canonicalize()
                .with_context(|| format!("resolve open Codex rollout {}", path.display()))?;
            if !path.starts_with(&sessions) || !is_rollout_file(&path) {
                continue;
            }
            let metadata = read_metadata(&path)?;
            if metadata.parent_thread_id.is_some() {
                continue;
            }
            let Some(thread_id) = metadata
                .thread_id
                .filter(|thread_id| self.root_rollouts.contains_key(thread_id))
            else {
                continue;
            };
            thread_ids.insert(thread_id);
            if thread_ids.len() > 1 {
                return Ok(None);
            }
        }
        Ok(thread_ids.into_iter().next())
    }

    fn discover(&mut self, path: PathBuf, now_ms: u64, retention_ms: u64) {
        if self.rollouts.contains_key(&path) {
            return;
        }
        if self.ignored.contains(&path) {
            return;
        }
        let Ok(file_metadata) = fs::metadata(&path) else {
            return;
        };
        let length = file_metadata.len();
        if self.failed_lengths.get(&path) == Some(&length) {
            return;
        }
        if let Some(previous_length) = self.expired_lengths.get(&path).copied()
            && fs::metadata(&path).map(|metadata| metadata.len()).ok() == Some(previous_length)
        {
            return;
        }
        self.expired_lengths.remove(&path);
        let modified_at_ms = file_metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64);
        if modified_at_ms.is_some_and(|modified| {
            now_ms.saturating_sub(modified) >= ACTIVE_STALE_MS.saturating_add(retention_ms)
        }) {
            self.expired_lengths.insert(path, length);
            return;
        }
        let Ok(metadata) = read_metadata(&path) else {
            self.failed_lengths.insert(path, length);
            return;
        };
        self.failed_lengths.remove(&path);
        if metadata.parent_thread_id.is_none() {
            if let Some(thread_id) = metadata.thread_id.clone() {
                self.root_rollouts.insert(
                    thread_id,
                    RootRollout {
                        cwd: metadata.cwd.clone(),
                        started_at_ms: metadata.started_at_ms,
                    },
                );
            }
            self.ignored.insert(path);
            return;
        }
        if metadata.thread_source.as_deref() != Some("subagent") {
            self.ignored.insert(path);
            return;
        }
        match CachedRollout::open(metadata) {
            Ok(cached) => {
                self.rollouts.insert(path, cached);
            }
            Err(_) => {
                self.failed_lengths.insert(path, length);
            }
        }
    }
}

pub fn collect_rollouts(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read Codex sessions directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_rollouts(&path, files)?;
        } else if is_rollout_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn collect_recent_rollouts(
    sessions: &Path,
    now_ms: u64,
    retention_ms: u64,
) -> Result<Vec<PathBuf>> {
    if !sessions.exists() {
        return Ok(Vec::new());
    }
    let mut day_directories = Vec::new();
    for year in directories(sessions)? {
        for month in directories(&year)? {
            day_directories.extend(directories(&month)?);
        }
    }
    day_directories.sort();
    let keep_from = day_directories.len().saturating_sub(RECENT_SESSION_DAYS);
    let mut paths = Vec::new();
    for (index, directory) in day_directories.iter().enumerate() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file() || !is_rollout_file(&path) {
                continue;
            }
            let recently_modified = entry
                .metadata()?
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64)
                .is_some_and(|modified| {
                    now_ms.saturating_sub(modified) < ACTIVE_STALE_MS.saturating_add(retention_ms)
                });
            if index >= keep_from || recently_modified {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

fn directories(parent: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(parent)?
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| entry.path())
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn is_rollout_file(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

pub fn read_metadata(path: &Path) -> Result<RolloutMetadata> {
    let file = File::open(path)?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line)?;
    let event: Value = serde_json::from_str(&line)?;
    if event.get("type").and_then(Value::as_str) != Some("session_meta") {
        bail!("rollout does not start with session metadata");
    }
    let payload = &event["payload"];
    let timestamp = payload
        .get("timestamp")
        .or_else(|| event.get("timestamp"))
        .and_then(Value::as_str)
        .context("session metadata has no timestamp")?;
    let spawn = payload
        .get("source")
        .and_then(|source| source.get("subagent"))
        .and_then(|subagent| subagent.get("thread_spawn"));
    let name = spawn
        .and_then(|spawn| spawn.get("agent_nickname"))
        .and_then(Value::as_str)
        .or_else(|| {
            spawn
                .and_then(|spawn| spawn.get("agent_role"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            payload
                .get("source")
                .and_then(|source| source.get("subagent"))
                .filter(|subagent| subagent.is_string())
                .and_then(Value::as_str)
        })
        .or_else(|| payload.get("agent_nickname").and_then(Value::as_str))
        .map(str::to_string);
    Ok(RolloutMetadata {
        path: path.to_path_buf(),
        thread_id: payload
            .get("id")
            .or_else(|| payload.get("thread_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_thread_id: payload
            .get("parent_thread_id")
            .or_else(|| spawn.and_then(|spawn| spawn.get("parent_thread_id")))
            .and_then(Value::as_str)
            .map(str::to_string),
        cwd: payload
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        started_at_ms: parse_rfc3339_ms(timestamp)?,
        thread_source: payload
            .get("thread_source")
            .and_then(Value::as_str)
            .map(str::to_string),
        name,
        agent_path: spawn
            .and_then(|spawn| spawn.get("agent_path"))
            .and_then(Value::as_str)
            .map(str::to_string),
        depth: spawn
            .and_then(|spawn| spawn.get("depth"))
            .and_then(Value::as_u64)
            .and_then(|depth| u32::try_from(depth).ok()),
        process_backed: payload
            .get("source")
            .and_then(|source| source.get("subagent"))
            .is_some_and(Value::is_string),
    })
}

pub fn normalize_name(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['_', ' '], "-")
}

pub fn resume_thread_id_from_argv(arguments: &[OsString]) -> Option<String> {
    let values = arguments
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    resume_thread_id(values.iter().map(String::as_str))
}

pub fn resume_thread_id_from_processes(processes: &str) -> Option<String> {
    let mut matches = processes.lines().filter_map(|process| {
        resume_thread_id(
            process
                .split_whitespace()
                .map(|value| value.trim_matches(|character| matches!(character, '\'' | '"'))),
        )
    });
    let thread_id = matches.next()?;
    matches
        .all(|candidate| candidate == thread_id)
        .then_some(thread_id)
}

pub fn codex_program_from_processes(processes: &str) -> bool {
    processes.lines().any(|process| {
        process
            .split_whitespace()
            .map(|value| value.trim_matches(|character| matches!(character, '\'' | '"')))
            .any(is_codex_program)
    })
}

fn resume_thread_id<'a>(arguments: impl Iterator<Item = &'a str>) -> Option<String> {
    let arguments = arguments.collect::<Vec<_>>();
    let resume = resume_subcommand(&arguments)?;
    arguments[resume + 1..]
        .iter()
        .find(|argument| is_uuid(argument))
        .map(|argument| argument.to_string())
}

fn resume_subcommand(arguments: &[&str]) -> Option<usize> {
    let codex = arguments
        .iter()
        .position(|argument| is_codex_program(argument))?;
    let resume = codex_subcommand(arguments, codex + 1)?;
    if arguments.get(resume).copied() != Some("resume") {
        return None;
    }
    Some(resume)
}

fn codex_subcommand(arguments: &[&str], mut index: usize) -> Option<usize> {
    while let Some(argument) = arguments.get(index).copied() {
        if argument == "--" {
            return (index + 1 < arguments.len()).then_some(index + 1);
        }
        if !argument.starts_with('-') || argument == "-" {
            return Some(index);
        }
        if codex_global_option_takes_value(argument) && !option_contains_value(argument) {
            index = index.checked_add(2)?;
        } else {
            index = index.checked_add(1)?;
        }
    }
    None
}

fn codex_global_option_takes_value(argument: &str) -> bool {
    matches!(
        argument,
        "-c" | "--config"
            | "--enable"
            | "--disable"
            | "--remote"
            | "--remote-auth-token-env"
            | "-i"
            | "--image"
            | "-m"
            | "--model"
            | "--local-provider"
            | "-p"
            | "--profile"
            | "-s"
            | "--sandbox"
            | "-C"
            | "--cd"
            | "--add-dir"
            | "-a"
            | "--ask-for-approval"
    )
}

fn option_contains_value(argument: &str) -> bool {
    argument.starts_with("--") && argument.contains('=')
        || argument.starts_with("-c") && argument.len() > 2
        || argument.starts_with("-i") && argument.len() > 2
        || argument.starts_with("-m") && argument.len() > 2
        || argument.starts_with("-p") && argument.len() > 2
        || argument.starts_with("-s") && argument.len() > 2
        || argument.starts_with("-C") && argument.len() > 2
        || argument.starts_with("-a") && argument.len() > 2
}

fn is_codex_program(value: &str) -> bool {
    let file_name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value);
    matches!(file_name, "codex" | "codex.exe" | "codex.js" | "codex.mjs")
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 || [8, 13, 18, 23].iter().any(|index| bytes[*index] != b'-') {
        return false;
    }
    bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

pub fn parse_rfc3339_ms(value: &str) -> Result<u64> {
    if value.len() < 20 {
        bail!("invalid RFC3339 timestamp");
    }
    let year = parse_time_part(value, 0, 4)? as i64;
    let month = parse_time_part(value, 5, 7)? as i64;
    let day = parse_time_part(value, 8, 10)? as i64;
    let hour = parse_time_part(value, 11, 13)? as i64;
    let minute = parse_time_part(value, 14, 16)? as i64;
    let second = parse_time_part(value, 17, 19)? as i64;
    let mut index = 19;
    let mut milliseconds = 0i64;
    if value.as_bytes().get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while value.as_bytes().get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let fraction = &value[start..index];
        let first_three = fraction.chars().take(3).collect::<String>();
        milliseconds = format!("{first_three:0<3}")
            .parse::<i64>()
            .context("parse timestamp fraction")?;
    }
    let offset_seconds = match value.as_bytes().get(index) {
        Some(b'Z') => 0i64,
        Some(sign @ (b'+' | b'-')) => {
            let offset_hour = parse_time_part(value, index + 1, index + 3)? as i64;
            let offset_minute = parse_time_part(value, index + 4, index + 6)? as i64;
            let offset = offset_hour * 3_600 + offset_minute * 60;
            if *sign == b'+' { offset } else { -offset }
        }
        _ => bail!("invalid RFC3339 timezone"),
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds;
    u64::try_from(seconds * 1_000 + milliseconds).context("timestamp predates Unix epoch")
}

fn parse_time_part(value: &str, start: usize, end: usize) -> Result<u32> {
    value
        .get(start..end)
        .context("invalid RFC3339 timestamp")?
        .parse()
        .context("invalid RFC3339 timestamp")
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_event(file: &mut File, event: Value) {
        writeln!(file, "{event}").unwrap();
        file.flush().unwrap();
    }

    fn write_thread_rollout(root: &Path, id: &str, parent: &str) -> PathBuf {
        let day = root.join("2026/07/26");
        fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-{id}.jsonl"));
        let mut file = File::create(&path).unwrap();
        write_event(
            &mut file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:24:56.218Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "session_id": parent,
                    "parent_thread_id": parent,
                    "thread_source": "subagent",
                    "cwd": "/work",
                    "timestamp": "2026-07-26T14:24:56.218Z",
                    "source": {
                        "subagent": {
                            "thread_spawn": {
                                "parent_thread_id": parent,
                                "depth": 1,
                                "agent_path": "/root/validator",
                                "agent_nickname": "Worker"
                            }
                        }
                    }
                }
            }),
        );
        write_event(
            &mut file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:24:56.417Z",
                "type": "event_msg",
                "payload": {"type": "task_started"}
            }),
        );
        path
    }

    fn write_root_rollout(root: &Path, id: &str) -> PathBuf {
        let day = root.join("2026/07/26");
        fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-{id}.jsonl"));
        let mut file = File::create(&path).unwrap();
        write_event(
            &mut file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:24:55.000Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "thread_source": "user",
                    "cwd": "/work",
                    "timestamp": "2026-07-26T14:24:55.000Z",
                    "source": "cli"
                }
            }),
        );
        path
    }

    #[test]
    fn parses_nested_thread_spawn_metadata() {
        let directory = tempdir().unwrap();
        let path = write_thread_rollout(
            directory.path(),
            "01800000-0000-7000-8000-000000000002",
            "01800000-0000-7000-8000-000000000001",
        );

        let metadata = read_metadata(&path).unwrap();

        assert_eq!(
            metadata.thread_id.as_deref(),
            Some("01800000-0000-7000-8000-000000000002")
        );
        assert_eq!(
            metadata.parent_thread_id.as_deref(),
            Some("01800000-0000-7000-8000-000000000001")
        );
        assert_eq!(metadata.name.as_deref(), Some("Worker"));
        assert_eq!(metadata.agent_path.as_deref(), Some("/root/validator"));
        assert_eq!(metadata.depth, Some(1));
        assert!(!metadata.process_backed);
    }

    #[test]
    fn null_nickname_falls_back_to_agent_role() {
        let directory = tempdir().unwrap();
        let path = write_thread_rollout(
            directory.path(),
            "01800000-0000-7000-8000-000000000002",
            "01800000-0000-7000-8000-000000000001",
        );
        let contents = fs::read_to_string(&path).unwrap();
        let updated = contents.replacen(
            "\"agent_nickname\":\"Worker\"",
            "\"agent_nickname\":null,\"agent_role\":\"review\"",
            1,
        );
        fs::write(&path, updated).unwrap();

        let metadata = read_metadata(&path).unwrap();

        assert_eq!(metadata.name.as_deref(), Some("review"));
    }

    #[test]
    fn agent_path_is_not_exposed_as_a_subagent_name() {
        let directory = tempdir().unwrap();
        let path = write_thread_rollout(
            directory.path(),
            "01800000-0000-7000-8000-000000000002",
            "01800000-0000-7000-8000-000000000001",
        );
        let contents = fs::read_to_string(&path).unwrap();
        let updated = contents.replacen("\"agent_nickname\":\"Worker\",", "", 1);
        fs::write(&path, updated).unwrap();

        let metadata = read_metadata(&path).unwrap();

        assert_eq!(metadata.name, None);
        assert_eq!(metadata.agent_path.as_deref(), Some("/root/validator"));
    }

    #[test]
    fn tracker_remembers_recent_root_rollout_identity() {
        let directory = tempdir().unwrap();
        write_root_rollout(directory.path(), "root");
        let now = parse_rfc3339_ms("2026-07-26T14:25:03.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));

        assert!(tracker.scan(now, 30_000).is_empty());
        assert_eq!(
            tracker.root_rollouts().get("root"),
            Some(&RootRollout {
                cwd: "/work".into(),
                started_at_ms: parse_rfc3339_ms("2026-07-26T14:24:55.000Z").unwrap(),
            })
        );
    }

    #[test]
    fn process_owned_rollout_recovers_only_a_known_root_identity() {
        let directory = tempdir().unwrap();
        let known = write_root_rollout(directory.path(), "known-root");
        let child = write_thread_rollout(directory.path(), "child", "known-root");
        let now = parse_rfc3339_ms("2026-07-26T14:25:03.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));
        tracker.scan(now, 30_000);
        let unknown = write_root_rollout(directory.path(), "unknown-root");

        assert_eq!(
            tracker
                .root_thread_id_from_process_rollouts([
                    child.as_path(),
                    unknown.as_path(),
                    known.as_path(),
                ])
                .unwrap()
                .as_deref(),
            Some("known-root")
        );
    }

    #[test]
    fn process_owned_rollouts_refuse_multiple_known_root_identities() {
        let directory = tempdir().unwrap();
        let first = write_root_rollout(directory.path(), "first-root");
        let second = write_root_rollout(directory.path(), "second-root");
        let now = parse_rfc3339_ms("2026-07-26T14:25:03.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));
        tracker.scan(now, 30_000);

        assert_eq!(
            tracker
                .root_thread_id_from_process_rollouts([first.as_path(), second.as_path()])
                .unwrap(),
            None
        );
    }

    #[test]
    fn process_owned_rollouts_fail_closed_on_invalid_session_metadata() {
        let directory = tempdir().unwrap();
        let known = write_root_rollout(directory.path(), "known-root");
        let now = parse_rfc3339_ms("2026-07-26T14:25:03.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));
        tracker.scan(now, 30_000);
        let invalid = directory.path().join("2026/07/26/rollout-invalid.jsonl");
        fs::write(&invalid, "{}\n").unwrap();

        assert!(
            tracker
                .root_thread_id_from_process_rollouts([known.as_path(), invalid.as_path()])
                .is_err()
        );
    }

    #[test]
    fn discovery_includes_recently_modified_rollout_from_an_older_date() {
        let directory = tempdir().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        for day in 1..=9 {
            let path = directory
                .path()
                .join(format!("2026/07/{day:02}/rollout-{day}.jsonl"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let file = File::create(path).unwrap();
            file.set_times(
                std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH),
            )
            .unwrap();
        }
        let old_active = directory.path().join("2026/07/01/rollout-1.jsonl");
        File::options()
            .write(true)
            .open(&old_active)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
            .unwrap();

        let discovered = collect_recent_rollouts(directory.path(), now, 30_000).unwrap();

        assert!(discovered.contains(&old_active));
        assert!(!discovered.contains(&directory.path().join("2026/07/02/rollout-2.jsonl")));
        assert_eq!(discovered.len(), 8);
    }

    #[test]
    fn tracker_follows_active_complete_and_retained_states() {
        let directory = tempdir().unwrap();
        let path = write_thread_rollout(directory.path(), "child", "parent");
        let started = parse_rfc3339_ms("2026-07-26T14:24:56.417Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));

        let active = tracker.scan(started, 30_000);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name.as_deref(), Some("Worker"));
        assert_eq!(active[0].finished_at_ms, None);

        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        write_event(
            &mut file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:01.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
        );
        let finished = parse_rfc3339_ms("2026-07-26T14:25:01.000Z").unwrap();
        let retained = tracker.scan(finished + 29_999, 30_000);
        assert_eq!(retained[0].finished_at_ms, Some(finished));
        assert!(tracker.scan(finished + 30_000, 30_000).is_empty());
    }

    #[test]
    fn completed_ancestor_is_retained_while_nested_thread_is_active() {
        let directory = tempdir().unwrap();
        let parent_path = write_thread_rollout(directory.path(), "parent", "root");
        let child_path = write_thread_rollout(directory.path(), "child", "parent");
        let mut parent_file = fs::OpenOptions::new()
            .append(true)
            .open(parent_path)
            .unwrap();
        write_event(
            &mut parent_file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:01.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
        );
        let finished = parse_rfc3339_ms("2026-07-26T14:25:01.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));

        let active = tracker.scan(finished + 30_000, 30_000);

        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|thread| thread.thread_id == "parent"));
        assert!(
            active
                .iter()
                .any(|thread| thread.thread_id == "child" && thread.finished_at_ms.is_none())
        );

        let mut child_file = fs::OpenOptions::new()
            .append(true)
            .open(child_path)
            .unwrap();
        write_event(
            &mut child_file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:32.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
        );
        assert!(tracker.scan(finished + 61_000, 30_000).is_empty());
    }

    #[test]
    fn completed_ancestor_is_retained_while_nested_completion_is_visible() {
        let directory = tempdir().unwrap();
        let parent_path = write_thread_rollout(directory.path(), "parent", "root");
        let child_path = write_thread_rollout(directory.path(), "child", "parent");
        let mut parent_file = fs::OpenOptions::new()
            .append(true)
            .open(parent_path)
            .unwrap();
        write_event(
            &mut parent_file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:01.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
        );
        let mut child_file = fs::OpenOptions::new()
            .append(true)
            .open(child_path)
            .unwrap();
        write_event(
            &mut child_file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:32.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
        );
        let now = parse_rfc3339_ms("2026-07-26T14:25:40.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));

        let retained = tracker.scan(now, 30_000);

        assert_eq!(retained.len(), 2);
        assert!(retained.iter().any(|thread| thread.thread_id == "parent"));
        assert!(retained.iter().any(|thread| thread.thread_id == "child"));
    }

    #[test]
    fn later_task_start_reactivates_a_thread() {
        let directory = tempdir().unwrap();
        let path = write_thread_rollout(directory.path(), "child", "parent");
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        for event in [
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:01.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:02.000Z",
                "type": "event_msg",
                "payload": {"type": "task_started"}
            }),
        ] {
            write_event(&mut file, event);
        }
        let now = parse_rfc3339_ms("2026-07-26T14:25:03.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));

        let threads = tracker.scan(now, 30_000);

        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].finished_at_ms, None);
    }

    #[test]
    fn expired_thread_is_rediscovered_after_a_new_task() {
        let directory = tempdir().unwrap();
        let path = write_thread_rollout(directory.path(), "child", "parent");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write_event(
            &mut file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:01.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
        );
        let finished = parse_rfc3339_ms("2026-07-26T14:25:01.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));
        assert!(tracker.scan(finished + 30_000, 30_000).is_empty());

        write_event(
            &mut file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:32.000Z",
                "type": "event_msg",
                "payload": {"type": "task_started"}
            }),
        );
        let active = tracker.scan(finished + 31_000, 30_000);

        assert_eq!(active.len(), 1);
        assert_eq!(active[0].finished_at_ms, None);
    }

    #[test]
    fn partial_session_metadata_is_retried_after_file_growth() {
        let directory = tempdir().unwrap();
        let day = directory.path().join("2026/07/26");
        fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout-child.jsonl");
        File::create(&path).unwrap();
        let now = parse_rfc3339_ms("2026-07-26T14:25:03.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));
        assert!(tracker.scan(now, 30_000).is_empty());

        write_thread_rollout(directory.path(), "child", "parent");
        let threads = tracker.scan(now + DISCOVERY_INTERVAL_MS, 30_000);

        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, "child");
    }

    #[test]
    fn unfinished_thread_eventually_expires_as_abandoned() {
        let directory = tempdir().unwrap();
        write_thread_rollout(directory.path(), "child", "parent");
        let started = parse_rfc3339_ms("2026-07-26T14:24:56.417Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));

        let active = tracker.scan(started + ACTIVE_STALE_MS - 1, 30_000);
        assert_eq!(active[0].finished_at_ms, None);
        let retained = tracker.scan(started + ACTIVE_STALE_MS + 29_999, 30_000);
        assert_eq!(retained[0].finished_at_ms, Some(started + ACTIVE_STALE_MS));
        assert!(
            tracker
                .scan(started + ACTIVE_STALE_MS + 30_000, 30_000)
                .is_empty()
        );
    }

    #[test]
    fn initial_lifecycle_scan_reads_the_bounded_tail() {
        let directory = tempdir().unwrap();
        let path = write_thread_rollout(directory.path(), "child", "parent");
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        let padding_start = file.metadata().unwrap().len();
        let padding = "é".repeat(INITIAL_TAIL_BYTES as usize / 2 + 256);
        writeln!(file, "{padding}").unwrap();
        write_event(
            &mut file,
            serde_json::json!({
                "timestamp": "2026-07-26T14:25:02.000Z",
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            }),
        );
        let mut offset = file.metadata().unwrap().len() - INITIAL_TAIL_BYTES;
        if (offset - padding_start).is_multiple_of(2) {
            write!(file, "x").unwrap();
            file.flush().unwrap();
            offset += 1;
        }
        assert_eq!((offset - padding_start) % 2, 1);
        let finished = parse_rfc3339_ms("2026-07-26T14:25:02.000Z").unwrap();
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));

        let threads = tracker.scan(finished, 30_000);

        assert_eq!(threads[0].finished_at_ms, Some(finished));
    }

    #[test]
    fn parses_rfc3339_milliseconds_and_offsets() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_rfc3339_ms("2026-07-26T02:30:32.019Z").unwrap(),
            1_785_033_032_019
        );
        assert_eq!(
            parse_rfc3339_ms("2026-07-26T04:30:32.019+02:00").unwrap(),
            1_785_033_032_019
        );
    }

    #[test]
    fn extracts_only_a_resume_session_uuid() {
        let id = "01800000-0000-7000-8000-000000000001";
        assert_eq!(
            resume_thread_id_from_processes(&format!(
                "tmux-agent run -- codex resume {id}\nnode /opt/bin/codex resume {id}"
            ))
            .as_deref(),
            Some(id)
        );
        assert!(
            resume_thread_id_from_processes("codex --prompt 01800000-0000-7000-8000-000000000001")
                .is_none()
        );
        assert!(
            resume_thread_id_from_processes(&format!("codex exec please run codex resume {id}"))
                .is_none()
        );
        assert!(resume_thread_id_from_processes(&format!("codex exec resume {id}")).is_none());
        assert_eq!(
            resume_thread_id_from_argv(&[
                OsString::from("codex"),
                OsString::from("--model"),
                OsString::from("gpt-5"),
                OsString::from("-C"),
                OsString::from("/work"),
                OsString::from("-a"),
                OsString::from("never"),
                OsString::from("resume"),
                OsString::from(id),
            ])
            .as_deref(),
            Some(id)
        );
        assert_eq!(
            resume_thread_id_from_argv(&[
                OsString::from("codex"),
                OsString::from("--profile=work"),
                OsString::from("resume"),
                OsString::from(id),
            ])
            .as_deref(),
            Some(id)
        );
    }

    #[test]
    fn recognizes_codex_program_without_parsing_flattened_option_values() {
        assert!(codex_program_from_processes(
            "codex --config instructions=a multi word value resume"
        ));
        assert!(codex_program_from_processes(
            "node /opt/bin/codex --profile=work resume --last"
        ));
        assert!(!codex_program_from_processes("node /opt/bin/pi"));
    }
}
