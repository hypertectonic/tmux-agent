//! Read only identity and lifecycle metadata from locally owned Claude sessions.
use crate::model::{AgentRecord, AgentState, Attention, EvidenceSource, SubagentInfo};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const POLL_MS: u64 = 1_000;
const WORKING_MS: u64 = 30_000;
const STALE_MS: u64 = 30 * 60 * 1_000;
const FINISHED_MS: u64 = 30_000;
const TAIL_BYTES: u64 = 256 * 1024;
const MAX_CHILDREN: usize = 128;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    pid: u32,
    session_id: String,
    cwd: String,
    proc_start: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChildName {
    agent_type: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    session_id: Option<String>,
    agent_id: Option<String>,
    is_sidechain: Option<bool>,
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    is_meta: Option<bool>,
    tool_ends_turn: Option<bool>,
    message: Option<Message>,
}

#[derive(Default, Deserialize)]
struct Message {
    stop_reason: Option<String>,
}

#[derive(Clone)]
struct Child {
    id: String,
    name: Option<String>,
    started: u64,
    last_event: u64,
    finished: Option<u64>,
}

struct CachedChild {
    stamp: (u64, u64, i64, i64),
    offset: u64,
    retry_at: Option<u64>,
    child: Child,
}

struct Observation {
    parent_id: String,
    parent_process: Option<crate::model::ProcessIdentity>,
    provider_process: crate::model::ProcessIdentity,
    child: Child,
}

#[derive(Default)]
pub(crate) struct ChildTracker {
    home: Option<PathBuf>,
    last_scan: Option<u64>,
    files: HashMap<PathBuf, CachedChild>,
    observations: Vec<Observation>,
}

impl ChildTracker {
    pub(crate) fn from_environment() -> Self {
        Self {
            home: std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| dirs::home_dir().map(|home| home.join(".claude"))),
            ..Self::default()
        }
    }

    pub(crate) fn reconcile(
        &mut self,
        records: &mut HashMap<String, AgentRecord>,
        record_pids: &HashMap<String, HashSet<u32>>,
        process_args: &HashMap<u32, String>,
        now: u64,
    ) {
        self.reconcile_with(
            records,
            record_pids,
            process_args,
            now,
            crate::tmux::stable_process_start_ms,
        );
    }

    fn reconcile_with(
        &mut self,
        records: &mut HashMap<String, AgentRecord>,
        record_pids: &HashMap<String, HashSet<u32>>,
        process_args: &HashMap<u32, String>,
        now: u64,
        process_start: impl Fn(u32) -> Option<u64>,
    ) {
        if self
            .last_scan
            .is_none_or(|last| now.saturating_sub(last) >= POLL_MS)
        {
            self.last_scan = Some(now);
            self.observations.clear();
            if !records
                .values()
                .any(|record| record.agent == "Claude" && record.subagent.is_none())
            {
                self.files.clear();
                return;
            }
            let mut visited = HashSet::new();
            if let Some(home) = self.home.clone() {
                let sessions = live_sessions(&home, process_args, &process_start);
                let mut session_counts = HashMap::<&str, usize>::new();
                for (session, _) in sessions.values() {
                    *session_counts.entry(&session.session_id).or_default() += 1;
                }
                let mut owners = HashMap::<String, Vec<(String, Session, u64)>>::new();
                for parent in records
                    .values()
                    .filter(|record| record.agent == "Claude" && record.subagent.is_none())
                {
                    let Some(pids) = record_pids.get(&parent.id) else {
                        continue;
                    };
                    let candidates: Vec<_> =
                        pids.iter().filter_map(|pid| sessions.get(pid)).collect();
                    // A nested CLI or two competing registry records must not be assigned by cwd.
                    if candidates.len() != 1 {
                        continue;
                    }
                    let (session, start) = candidates[0];
                    if session_counts[session.session_id.as_str()] != 1 {
                        continue;
                    }
                    owners.entry(session.session_id.clone()).or_default().push((
                        parent.id.clone(),
                        session.clone(),
                        *start,
                    ));
                }
                for owners in owners.into_values().filter(|owners| owners.len() == 1) {
                    let (parent_id, session, start) = owners.into_iter().next().unwrap();
                    let parent_process = records[&parent_id].process;
                    let provider_process = crate::model::ProcessIdentity {
                        pid: session.pid,
                        started_at_ms: Some(start),
                    };
                    let project = project_name(&session.cwd);
                    let directory = home
                        .join("projects")
                        .join(project)
                        .join(&session.session_id)
                        .join("subagents");
                    let Ok(entries) = fs::read_dir(&directory) else {
                        continue;
                    };
                    // Do not search other projects or replay the full parent transcript.
                    let mut paths: Vec<_> = entries
                        .take(4096)
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.extension()
                                .is_some_and(|extension| extension == "jsonl")
                        })
                        .filter_map(|path| {
                            fs::symlink_metadata(&path)
                                .ok()
                                .filter(|meta| meta.is_file())
                                .map(|meta| (meta.mtime(), path))
                        })
                        .collect();
                    paths.sort();
                    for (_, path) in paths.into_iter().rev().take(MAX_CHILDREN) {
                        let Some(agent_id) = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .and_then(|name| name.strip_prefix("agent-"))
                            .and_then(|name| name.strip_suffix(".jsonl"))
                            .filter(|id| valid_id(id))
                        else {
                            continue;
                        };
                        let id = format!(
                            "{parent_id}/claude/{}/{start}/{agent_id}",
                            session.session_id
                        );
                        visited.insert(path.clone());
                        if let Some(child) = refresh_child(
                            &mut self.files,
                            &path,
                            &session.session_id,
                            agent_id,
                            &id,
                            now,
                        ) && child.last_event >= start
                        {
                            self.observations.push(Observation {
                                parent_id: parent_id.clone(),
                                parent_process,
                                provider_process,
                                child,
                            });
                        }
                    }
                }
            }
            self.files.retain(|path, _| visited.contains(path));
        }
        let mut current_starts = HashMap::new();
        for Observation {
            parent_id,
            parent_process,
            provider_process,
            child,
        } in &self.observations
        {
            let Some(parent) = records.get(parent_id).filter(|record| {
                record.agent == "Claude"
                    && record.subagent.is_none()
                    && record.process == *parent_process
            }) else {
                continue;
            };
            // Terminal and unmatched owned-PTY records have no foreground-group
            // identity. Keep their verified provider lifetime separate instead.
            if !record_pids
                .get(parent_id)
                .is_some_and(|pids| pids.contains(&provider_process.pid))
                || *current_starts
                    .entry(provider_process.pid)
                    .or_insert_with(|| process_start(provider_process.pid))
                    != provider_process.started_at_ms
            {
                continue;
            }
            if now.saturating_sub(child.last_event) >= STALE_MS
                || child
                    .finished
                    .is_some_and(|finished| now.saturating_sub(finished) >= FINISHED_MS)
            {
                continue;
            }
            let mut record = parent.clone();
            record.id = child.id.clone();
            record.state = if child.finished.is_some() {
                AgentState::Idle
            } else if now.saturating_sub(child.last_event) < WORKING_MS {
                AgentState::Working
            } else {
                AgentState::Unknown
            };
            record.attention = match record.state {
                AgentState::Working => Attention::Working,
                AgentState::Idle => Attention::Done,
                _ => Attention::Unknown,
            };
            record.title = child.name.clone().unwrap_or_else(|| "Claude child".into());
            record.label = None;
            record.goal = None;
            record.detection = None;
            record.source = EvidenceSource::Process;
            record.changed_at_ms = child.finished.unwrap_or(child.last_event);
            record.seen = child.finished.is_none();
            record.subagent = Some(SubagentInfo {
                parent_id: parent_id.clone(),
                started_at_ms: child.started,
                finished_at_ms: child.finished,
                name: child.name.clone(),
                thread_id: None,
            });
            records.insert(record.id.clone(), record);
        }
    }
}

fn live_sessions(
    home: &Path,
    process_args: &HashMap<u32, String>,
    process_start: &impl Fn(u32) -> Option<u64>,
) -> HashMap<u32, (Session, u64)> {
    process_args
        .iter()
        .filter_map(|(pid, args)| {
            if crate::detect::detect(args, "", "")?.agent != "Claude" {
                return None;
            }
            let session: Session = read_json(
                &home.join("sessions").join(format!("{pid}.json")),
                16 * 1024,
            )?;
            let start = process_start(*pid)?;
            if session.pid != *pid
                || !valid_id(&session.session_id)
                || !Path::new(&session.cwd).is_absolute()
                || proc_start_ms(&session.proc_start)? / 1_000 != start / 1_000
            {
                return None;
            }
            Some((*pid, (session, start)))
        })
        .collect()
}

pub(crate) fn is_metadata_child(record: &AgentRecord) -> bool {
    record.agent == "Claude"
        && record.subagent.as_ref().is_some_and(|child| {
            record
                .id
                .strip_prefix(&child.parent_id)
                .is_some_and(|suffix| suffix.starts_with("/claude/"))
        })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn project_name(cwd: &str) -> String {
    cwd.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

// Claude's registry uses LC_ALL=C, TZ=UTC `ps -o lstart=` on macOS and Linux.
fn proc_start_ms(value: &str) -> Option<u64> {
    let fields: Vec<_> = value.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| *month == fields[1])?
        + 1;
    let day: u32 = fields[2].parse().ok()?;
    crate::codex::parse_rfc3339_ms(&format!("{}-{month:02}-{day:02}T{}Z", fields[4], fields[3]))
        .ok()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Option<T> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn refresh_child(
    cache: &mut HashMap<PathBuf, CachedChild>,
    path: &Path,
    session: &str,
    agent: &str,
    id: &str,
    now: u64,
) -> Option<Child> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        cache.remove(path);
        return None;
    }
    let stamp = (
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    );
    if let Some(cached) = cache.get(path).filter(|cached| {
        cached.stamp == stamp
            && cached.child.id == id
            && cached.retry_at.is_none_or(|retry_at| now < retry_at)
    }) {
        return Some(cached.child.clone());
    }
    let previous = cache.get(path).filter(|cached| {
        cached.child.id == id
            && cached.stamp.0 == stamp.0
            && (cached.stamp.1 < stamp.1 || cached.stamp == stamp && cached.retry_at.is_some())
    });
    let tail_start = metadata.len().saturating_sub(TAIL_BYTES);
    let offset = previous.map_or(tail_start, |cached| cached.offset.max(tail_start));
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let mut child = previous.map(|cached| cached.child.clone());
    let mut consumed = offset;
    let mut retry_at = None;
    let mut lines = bytes.split_inclusive(|b| *b == b'\n');
    if offset > 0
        && previous.is_none_or(|cached| offset != cached.offset)
        && let Some(partial) = lines.next()
    {
        consumed += partial.len() as u64;
    }
    for line in lines.filter(|line| line.ends_with(b"\n")) {
        consumed += line.len() as u64;
        let Ok(event) = serde_json::from_slice::<Event>(line) else {
            continue;
        };
        if event.session_id.as_deref() != Some(session)
            || event.agent_id.as_deref() != Some(agent)
            || event.is_sidechain != Some(true)
            || event.is_meta == Some(true)
            || !matches!(event.kind.as_deref(), Some("user" | "assistant"))
        {
            continue;
        }
        let Some(timestamp) = event
            .timestamp
            .as_deref()
            .and_then(|timestamp| crate::codex::parse_rfc3339_ms(timestamp).ok())
        else {
            continue;
        };
        if timestamp > now {
            // The transcript can advance after the scanner captures `now`.
            // Leave this line unread and reconsider it on a normal later poll.
            consumed -= line.len() as u64;
            retry_at = Some(timestamp);
            break;
        }
        let current = child.get_or_insert_with(|| Child {
            id: id.into(),
            name: None,
            started: timestamp,
            last_event: 0,
            finished: None,
        });
        if timestamp < current.last_event {
            continue;
        }
        if current.finished.is_some() {
            current.started = timestamp;
        }
        current.last_event = timestamp;
        current.finished = (event.tool_ends_turn == Some(true)
            || event
                .message
                .as_ref()
                .and_then(|message| message.stop_reason.as_deref())
                == Some("end_turn"))
        .then_some(timestamp);
    }
    let mut child = child?;
    child.name = read_json::<ChildName>(&path.with_extension("meta.json"), 16 * 1024)
        .map(|metadata| metadata.agent_type)
        .filter(|name| {
            !name.is_empty() && name.len() <= 80 && name.chars().all(|c| !c.is_control())
        });
    cache.insert(
        path.into(),
        CachedChild {
            stamp,
            offset: consumed,
            retry_at,
            child: child.clone(),
        },
    );
    Some(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::{TempDir, tempdir};

    const START: u64 = 1_782_864_000_000;

    fn parent(pid: u32) -> AgentRecord {
        serde_json::from_value(json!({
            "id":format!("host/default/%{pid}"), "host":"host", "server":"default",
            "pane_id":format!("%{pid}"), "pane_pid":pid,
            "process":{"pid":pid,"started_at_ms":START},
            "session_id":"$1","session_name":"work","window_id":"@1",
            "window_index":1,"window_name":"task","pane_index":0,
            "agent":"Claude","state":"idle","attention":"idle","source":"process",
            "title":"parent","cwd":"/project","visible":false,"seen":true,"changed_at_ms":START
        }))
        .unwrap()
    }

    struct Fixture {
        dir: TempDir,
        tracker: ChildTracker,
        parents: Vec<AgentRecord>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let tracker = ChildTracker {
                home: Some(dir.path().into()),
                ..ChildTracker::default()
            };
            let mut fixture = Self {
                dir,
                tracker,
                parents: Vec::new(),
            };
            fixture.add_parent(10, "root");
            fixture
        }
        fn add_parent(&mut self, pid: u32, session: &str) {
            let directory = self.dir.path().join("sessions");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(format!("{pid}.json")), json!({
                "pid":pid,"sessionId":session,"cwd":"/project", "procStart":"Wed Jul  1 00:00:00 2026"
            }).to_string()).unwrap();
            self.parents.push(parent(pid));
        }
        fn path(&self, session: &str, agent: &str) -> PathBuf {
            self.dir
                .path()
                .join("projects/-project")
                .join(session)
                .join("subagents")
                .join(format!("agent-{agent}.jsonl"))
        }
        fn append(&self, session: &str, agent: &str, second: u64, terminal: bool) {
            let path = self.path(session, agent);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                file,
                "{}",
                json!({"sessionId":session,"agentId":agent,"isSidechain":true,
                "timestamp":format!("2026-07-01T00:00:{second:02}.000Z"), "type":"user",
                "toolEndsTurn":terminal,"message":{"content":"private prompt never serialized"}})
            )
            .unwrap();
            fs::write(
                path.with_extension("meta.json"),
                json!({"agentType":"reviewer","description":"private task"}).to_string(),
            )
            .unwrap();
        }
        fn scan(&mut self, elapsed: u64) -> HashMap<String, AgentRecord> {
            let mut records = self
                .parents
                .iter()
                .cloned()
                .map(|record| (record.id.clone(), record))
                .collect();
            let pids = self
                .parents
                .iter()
                .map(|record| (record.id.clone(), HashSet::from([record.pane_pid])))
                .collect();
            let args = self
                .parents
                .iter()
                .map(|record| (record.pane_pid, "claude".into()))
                .collect();
            self.tracker
                .reconcile_with(&mut records, &pids, &args, START + elapsed, |_| Some(START));
            records
        }
    }

    fn children(records: &HashMap<String, AgentRecord>) -> Vec<&AgentRecord> {
        records
            .values()
            .filter(|record| is_metadata_child(record))
            .collect()
    }

    #[test]
    fn registry_identity_links_duplicate_child_names_and_rejects_ambiguous_sessions() {
        assert_eq!(proc_start_ms("Wed Jul  1 00:00:00 2026"), Some(START));
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, false);
        fixture.append("root", "two", 1, false);
        fixture.add_parent(20, "other");
        fixture.append("other", "one", 1, false);
        let records = fixture.scan(2_000);
        let rows = children(&records);
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| row.subagent.as_ref().unwrap().name.as_deref() == Some("reviewer"))
        );
        assert_eq!(
            rows.iter().map(|row| &row.id).collect::<HashSet<_>>().len(),
            3
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.subagent.as_ref().unwrap().parent_id == "host/default/%10")
                .count(),
            2
        );
        let wire = serde_json::to_string(&records).unwrap();
        assert!(!wire.contains("private prompt"));
        assert!(!wire.contains("private task"));
        fixture.add_parent(30, "root");
        assert_eq!(children(&fixture.scan(3_000)).len(), 1);
    }

    #[test]
    fn completion_resume_same_millisecond_and_unknown_expiry_are_distinct() {
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, false);
        let records = fixture.scan(1_000);
        let id = children(&records)[0].id.clone();
        fixture.append("root", "one", 1, true);
        let records = fixture.scan(2_000);
        let child = &records[&id];
        assert_eq!(child.attention, Attention::Done);
        assert_eq!(
            child.subagent.as_ref().unwrap().finished_at_ms,
            Some(START + 1_000)
        );
        fixture.append("root", "one", 3, false);
        let records = fixture.scan(3_000);
        assert_eq!(records[&id].state, AgentState::Working);
        assert_eq!(
            records[&id].subagent.as_ref().unwrap().started_at_ms,
            START + 3_000
        );
        let records = fixture.scan(33_000);
        assert_eq!(records[&id].state, AgentState::Unknown);
        assert!(
            records[&id]
                .subagent
                .as_ref()
                .unwrap()
                .finished_at_ms
                .is_none()
        );
        assert!(children(&fixture.scan(STALE_MS + 3_000)).is_empty());
    }

    #[test]
    fn registry_replacement_missing_metadata_and_process_replacement_drop_children() {
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, false);
        assert_eq!(children(&fixture.scan(1_000)).len(), 1);
        fixture.parents[0].process.as_mut().unwrap().started_at_ms = Some(START + 500);
        assert!(children(&fixture.scan(1_500)).is_empty());
        fs::write(fixture.dir.path().join("sessions/10.json"),json!({"pid":10,"sessionId":"root","cwd":"/project","procStart":"Wed Jul  1 00:00:01 2026"}).to_string()).unwrap();
        assert!(children(&fixture.scan(2_000)).is_empty());
        fs::remove_file(fixture.dir.path().join("sessions/10.json")).unwrap();
        assert!(children(&fixture.scan(3_000)).is_empty());
    }

    #[test]
    fn bounded_tail_ignores_partial_and_oversized_lines_and_wrong_ownership() {
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, false);
        assert_eq!(children(&fixture.scan(1_000)).len(), 1);
        let path = fixture.path("root", "one");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let event=json!({"sessionId":"root","agentId":"one","isSidechain":true,"timestamp":"2026-07-01T00:00:02.000Z","type":"user","toolEndsTurn":true}).to_string();
        write!(file, "{event}").unwrap();
        assert_eq!(children(&fixture.scan(2_000))[0].state, AgentState::Working);
        writeln!(file).unwrap();
        assert_eq!(children(&fixture.scan(3_000))[0].attention, Attention::Done);
        file.write_all(&vec![b'x'; TAIL_BYTES as usize + 1])
            .unwrap();
        writeln!(file).unwrap();
        fixture.append("root", "one", 4, false);
        assert_eq!(children(&fixture.scan(4_000))[0].state, AgentState::Working);
        fs::write(&path,json!({"sessionId":"wrong","agentId":"one","isSidechain":true,"timestamp":"2026-07-01T00:00:05.000Z","type":"user"}).to_string()+"\n").unwrap();
        assert!(children(&fixture.scan(5_000)).is_empty());
        assert!(!valid_id("../escape"));
    }

    #[test]
    fn quiet_finished_child_expires_without_reappearing() {
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, true);
        assert_eq!(children(&fixture.scan(1_000))[0].attention, Attention::Done);
        assert!(children(&fixture.scan(31_000)).is_empty());
        assert!(children(&fixture.scan(32_000)).is_empty());
    }

    #[test]
    fn selection_resolves_parent_and_child_is_not_a_handoff_target() {
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, false);
        let records = fixture.scan(1_000);
        let child = children(&records)[0].clone();
        let mut snapshot = crate::model::Snapshot {
            agents: records.into_values().collect(),
            ..crate::model::Snapshot::default()
        };
        assert_eq!(
            crate::focus::parent_focus_record(&snapshot, &child)
                .unwrap()
                .id,
            "host/default/%10"
        );
        assert!(
            crate::handoff::resolve_target(
                &snapshot,
                Some(&child.id),
                &crate::handoff::TargetFilters::default()
            )
            .is_err()
        );
        snapshot.agents.retain(|record| record.subagent.is_some());
        assert!(crate::focus::parent_focus_record(&snapshot, &child).is_err());
    }

    #[test]
    fn no_extra_io_between_polls_and_many_metadata_siblings_do_not_hide_children() {
        let mut fixture = Fixture::new();
        for index in 0..70 {
            fixture.append("root", &format!("child-{index}"), 1, false);
        }
        assert_eq!(children(&fixture.scan(1_000)).len(), 70);
        fs::remove_file(fixture.dir.path().join("sessions/10.json")).unwrap();
        assert_eq!(children(&fixture.scan(1_500)).len(), 70);
        assert!(children(&fixture.scan(2_000)).is_empty());
    }

    #[test]
    fn resumed_session_on_replacement_process_requires_new_events() {
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, false);
        let old_records = fixture.scan(1_000);
        let old_id = children(&old_records)[0].id.clone();
        fixture.parents.clear();
        fixture.add_parent(20, "root");
        fixture.parents[0].process.as_mut().unwrap().started_at_ms = Some(START + 2_000);
        fs::write(fixture.dir.path().join("sessions/20.json"), json!({
            "pid":20, "sessionId":"root", "cwd":"/project", "procStart":"Wed Jul  1 00:00:02 2026"
        }).to_string()).unwrap();
        let scan = |fixture: &mut Fixture, now| {
            let parent = fixture.parents[0].clone();
            let mut records = HashMap::from([(parent.id.clone(), parent.clone())]);
            fixture.tracker.reconcile_with(
                &mut records,
                &HashMap::from([(parent.id, HashSet::from([20]))]),
                &HashMap::from([(20, "claude".into())]),
                START + now,
                |_| Some(START + 2_000),
            );
            records
        };
        assert!(children(&scan(&mut fixture, 2_000)).is_empty());
        fixture.append("root", "one", 3, false);
        let records = scan(&mut fixture, 3_000);
        assert_eq!(children(&records).len(), 1);
        assert_ne!(children(&records)[0].id, old_id);
    }

    #[test]
    fn terminal_and_owned_pty_parents_do_not_need_a_foreground_group_identity() {
        for id in ["host/terminal/tty1/10", "host/run/fixture-run"] {
            let mut fixture = Fixture::new();
            fixture.parents[0].id = id.into();
            fixture.parents[0].origin = crate::model::AgentOrigin::Terminal;
            fixture.parents[0].pane_id.clear();
            fixture.parents[0].process = None;
            fixture.append("root", "one", 1, false);
            let records = fixture.scan(1_000);
            let rows = children(&records);
            assert_eq!(rows.len(), 1, "parent {id}");
            assert_eq!(rows[0].subagent.as_ref().unwrap().parent_id, id);
            assert!(records[id].process.is_none());
            assert!(rows[0].process.is_none());
            let mut replacement = HashMap::from([(id.into(), fixture.parents[0].clone())]);
            fixture.tracker.reconcile_with(
                &mut replacement,
                &HashMap::from([(id.into(), HashSet::from([10]))]),
                &HashMap::from([(10, "claude".into())]),
                START + 1_500,
                |_| Some(START + 500),
            );
            assert!(
                children(&replacement).is_empty(),
                "cached provider lifetime for {id}"
            );
        }
    }

    #[test]
    fn completion_newer_than_scan_start_is_reconsidered_without_another_write() {
        let mut fixture = Fixture::new();
        fixture.append("root", "one", 1, false);
        assert_eq!(children(&fixture.scan(1_000))[0].state, AgentState::Working);
        fixture.append("root", "one", 3, true);
        assert_eq!(children(&fixture.scan(2_000))[0].state, AgentState::Working);
        // The file is unchanged here. The pending completion must not be lost
        // just because it was appended after the preceding scan began.
        let records = fixture.scan(4_000);
        assert_eq!(children(&records)[0].attention, Attention::Done);
        assert_eq!(
            children(&records)[0]
                .subagent
                .as_ref()
                .unwrap()
                .finished_at_ms,
            Some(START + 3_000)
        );
    }
}
