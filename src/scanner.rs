use crate::codex::{self, CodexOwnership, ReconciliationFrame, ThreadTracker};
use crate::config::Config;
use crate::detect;
use crate::detect::stabilize::{ObservationFreshness, StateTracker};
use crate::model::{
    AgentOrigin, AgentRecord, AgentState, Attention, EvidenceSource, GoalInfo, GoalState,
    PROTOCOL_VERSION, PersistedState, Snapshot, SubagentInfo,
};
use crate::runner::{self, RunnerState};
use crate::tmux::{ProcessSnapshot, TerminalJob, Tmux, is_server_missing};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUBAGENT_RETENTION_MS: u64 = 30_000;
const BACKGROUND_CAPTURE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureIdentity {
    pane_pid: u32,
    process_group: u32,
    process_started_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct CaptureCandidate<'a> {
    pane_id: &'a str,
    identity: CaptureIdentity,
    foreground: bool,
}

#[derive(Debug)]
struct CaptureEntry {
    identity: CaptureIdentity,
    attempted_at: Instant,
    screen: Option<String>,
}

#[derive(Debug, Default)]
struct CaptureCache {
    entries: HashMap<String, CaptureEntry>,
}

impl CaptureCache {
    fn due_panes<'a>(
        &mut self,
        candidates: impl IntoIterator<Item = CaptureCandidate<'a>>,
        now: Instant,
    ) -> Vec<String> {
        let mut eligible = HashSet::new();
        let mut due = Vec::new();
        for candidate in candidates {
            eligible.insert(candidate.pane_id.to_string());
            let should_capture = self.entries.get(candidate.pane_id).is_none_or(|entry| {
                entry.identity != candidate.identity
                    || candidate.foreground
                    || now.saturating_duration_since(entry.attempted_at)
                        >= BACKGROUND_CAPTURE_INTERVAL
            });
            if should_capture {
                if let Some(entry) = self.entries.get_mut(candidate.pane_id) {
                    if entry.identity == candidate.identity {
                        entry.attempted_at = now;
                    } else {
                        *entry = CaptureEntry {
                            identity: candidate.identity,
                            attempted_at: now,
                            screen: None,
                        };
                    }
                } else {
                    self.entries.insert(
                        candidate.pane_id.to_string(),
                        CaptureEntry {
                            identity: candidate.identity,
                            attempted_at: now,
                            screen: None,
                        },
                    );
                }
                due.push(candidate.pane_id.to_string());
            }
        }
        self.entries.retain(|pane_id, _| eligible.contains(pane_id));
        due
    }

    fn apply_results(&mut self, pane_ids: &[String], results: &HashMap<String, Result<String>>) {
        for pane_id in pane_ids {
            if let Some(entry) = self.entries.get_mut(pane_id) {
                entry.screen = results
                    .get(pane_id)
                    .and_then(|result| result.as_ref().ok())
                    .cloned();
            }
        }
    }

    fn screen(&self, pane_id: &str) -> Option<&str> {
        self.entries
            .get(pane_id)
            .and_then(|entry| entry.screen.as_deref())
    }
}

pub struct Scanner {
    tmux: Tmux,
    host: String,
    server: String,
    previous: HashMap<String, AgentRecord>,
    detection_state: StateTracker,
    terminal_cwds: HashMap<u32, String>,
    runner_directory: PathBuf,
    codex_threads: ThreadTracker,
    codex_ownership: CodexOwnership,
    record_starts: HashMap<String, (String, u64)>,
    captures: CaptureCache,
    revision: u64,
    tmux_server_observed: bool,
}

impl Scanner {
    pub fn new(
        config: &Config,
        tmux: Tmux,
        server_key: &str,
        runner_directory: PathBuf,
        persisted: Option<PersistedState>,
        tmux_server_observed: bool,
    ) -> Result<Self> {
        let host = config.host_name.clone().unwrap_or_else(discover_host);
        let server = config.server_name.clone().unwrap_or_else(|| {
            std::path::Path::new(server_key)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("default")
                .to_string()
        });
        let previous = persisted
            .filter(|state| {
                state.protocol == PROTOCOL_VERSION && state.host == host && state.server == server
            })
            .map(|state| {
                state
                    .agents
                    .into_iter()
                    .map(|agent| (agent.id.clone(), agent))
                    .collect()
            })
            .unwrap_or_default();
        let codex_ownership = CodexOwnership::new(&previous, &host, &server);
        Ok(Self {
            tmux,
            host,
            server,
            previous,
            detection_state: StateTracker::default(),
            terminal_cwds: HashMap::new(),
            runner_directory,
            codex_threads: ThreadTracker::from_environment(),
            codex_ownership,
            record_starts: HashMap::new(),
            captures: CaptureCache::default(),
            revision: 0,
            tmux_server_observed,
        })
    }

    pub fn scan(&mut self) -> Result<Snapshot> {
        // A daemon started without tmux still discovers ordinary terminals. Once its
        // configured tmux server has been observed, losing that server is a lifecycle event.
        let panes = match self.tmux.list_panes() {
            Ok(panes) => {
                self.tmux_server_observed = true;
                panes
            }
            Err(error) if is_server_missing(&error) && !self.tmux_server_observed => Vec::new(),
            Err(error) => return Err(error),
        };
        let processes = self.tmux.process_snapshot(&panes)?;
        let session_connections = self.tmux.session_connections(&processes)?;
        let runner_states = runner::load_states(&self.runner_directory, &processes.live_pids);
        let capture_candidates = panes
            .iter()
            .filter(|pane| !pane.is_agent_ui && !pane.dead)
            .filter(|pane| runner_for_pane(&runner_states, &processes, &pane.pane_id).is_none())
            .filter(|pane| {
                let process = processes
                    .panes
                    .get(&pane.pane_id)
                    .map(String::as_str)
                    .unwrap_or(&pane.current_command);
                detect::looks_like_agent(process)
            })
            .map(|pane| {
                let process_group = processes
                    .pane_groups
                    .get(&pane.pane_id)
                    .copied()
                    .unwrap_or(pane.pane_pid);
                CaptureCandidate {
                    pane_id: &pane.pane_id,
                    identity: CaptureIdentity {
                        pane_pid: pane.pane_pid,
                        process_group,
                        process_started_at_ms: processes
                            .process_started_at_ms
                            .get(&process_group)
                            .copied(),
                    },
                    foreground: pane.visible,
                }
            })
            .collect::<Vec<_>>();
        let capture_pane_ids = self.captures.due_panes(capture_candidates, Instant::now());
        let captured_screens = self.tmux.capture_visible_batch(&capture_pane_ids);
        self.captures
            .apply_results(&capture_pane_ids, &captured_screens);
        let now = now_ms();
        let mut next = HashMap::new();
        let mut record_pids = HashMap::<String, HashSet<u32>>::new();
        let mut record_thread_ids = HashMap::<String, String>::new();
        let mut subagent_names = HashMap::<String, String>::new();
        let mut claimed_runners = HashSet::new();

        for pane in panes
            .into_iter()
            .filter(|pane| !pane.is_agent_ui && !pane.dead)
        {
            let wrapped = runner_for_pane(&runner_states, &processes, &pane.pane_id);
            let process = processes
                .panes
                .get(&pane.pane_id)
                .map(String::as_str)
                .unwrap_or(&pane.current_command);
            if wrapped.is_none() && !detect::looks_like_agent(process) {
                continue;
            }
            let (mut detection, observation_freshness) = if let Some(wrapped) = wrapped {
                claimed_runners.insert(wrapped.run_id.clone());
                (wrapped.as_detection(), ObservationFreshness::Fresh)
            } else {
                let screen = self.captures.screen(&pane.pane_id).unwrap_or_default();
                let Some(mut detection) = detect::detect(process, &pane.title, screen) else {
                    continue;
                };
                if self.captures.screen(&pane.pane_id).is_none() {
                    preserve_on_capture_failure(&mut detection);
                }
                let freshness = if captured_screens
                    .get(&pane.pane_id)
                    .is_some_and(Result::is_ok)
                {
                    ObservationFreshness::Fresh
                } else {
                    ObservationFreshness::Replayed
                };
                (detection, freshness)
            };
            let id = format!("{}/{}/{}", self.host, self.server, pane.pane_id);
            let old = self.previous.get(&id);
            let identity = format!(
                "{}:{}",
                detection.agent,
                wrapped
                    .map(|state| state.run_id.as_str().to_string())
                    .unwrap_or_else(|| processes
                        .pane_groups
                        .get(&pane.pane_id)
                        .copied()
                        .unwrap_or(pane.pane_pid)
                        .to_string())
            );
            let observed_start = observed_process_start(
                processes
                    .pane_pids
                    .get(&pane.pane_id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[pane.pane_pid]),
                &processes.process_started_at_ms,
                now,
            );
            remember_record_start(&mut self.record_starts, &id, &identity, observed_start);
            detection = self.detection_state.stabilize_observation(
                &id,
                &identity,
                detection,
                old,
                now,
                observation_freshness,
            );
            let seen = next_seen(old, detection.state, pane.visible);
            let attention = attention(detection.state, seen);
            let goal = goal_lifecycle(old, detection.state, detection.goal, now);
            let changed_at_ms = old
                .filter(|old| old.state == detection.state && old.attention == attention)
                .map(|old| old.changed_at_ms)
                .unwrap_or(now);
            let codex_thread_id = detection
                .agent
                .eq_ignore_ascii_case("codex")
                .then(|| {
                    wrapped
                        .and_then(|state| runner_codex_thread_id(state, &processes.process_args))
                        .or_else(|| codex::resume_thread_id_from_processes(process))
                })
                .flatten();
            let cwd = wrapped
                .filter(|state| !state.cwd.is_empty())
                .map(|state| state.cwd.clone())
                .unwrap_or(pane.cwd);
            let title = collected_title(wrapped, &detection.agent, pane.title);
            let record = AgentRecord {
                id: id.clone(),
                host: self.host.clone(),
                server: self.server.clone(),
                pane_id: pane.pane_id,
                pane_pid: pane.pane_pid,
                session_id: pane.session_id,
                session_name: pane.session_name,
                window_id: pane.window_id,
                window_index: pane.window_index,
                window_name: pane.window_name,
                pane_index: pane.pane_index,
                agent: detection.agent,
                state: detection.state,
                attention,
                source: detection.source,
                title,
                label: pane.label,
                cwd,
                visible: pane.visible,
                seen,
                changed_at_ms,
                origin: AgentOrigin::Tmux,
                terminal: None,
                remote_alias: None,
                ssh_connection: None,
                session_connections: None,
                focus_target: None,
                goal,
                subagent: None,
                detection: detection.details,
            };
            record_pids.insert(
                id.clone(),
                pane_record_pids(
                    record.pane_pid,
                    processes.pane_pids.get(&record.pane_id).map(Vec::as_slice),
                    wrapped,
                ),
            );
            if let Some(thread_id) = codex_thread_id {
                record_thread_ids.insert(id.clone(), thread_id);
            }
            next.insert(id, record);
        }

        let terminals = processes
            .terminals
            .into_iter()
            .filter(|terminal| !terminal_belongs_to_runner(terminal, &runner_states))
            .filter_map(|terminal| {
                detect::detect(&terminal.processes, "", "").map(|mut detection| {
                    mark_process_only(&mut detection);
                    let connection = processes.ssh_connections.get(&terminal.leader_pid).cloned();
                    (terminal, detection, connection)
                })
            })
            .collect::<Vec<_>>();
        let terminal_pids = terminals
            .iter()
            .map(|(terminal, _, _)| terminal.leader_pid)
            .collect::<HashSet<_>>();
        let missing_cwds = terminal_pids
            .iter()
            .filter(|pid| !self.terminal_cwds.contains_key(pid))
            .copied()
            .collect::<Vec<_>>();
        self.terminal_cwds
            .extend(self.tmux.process_working_directories(&missing_cwds));
        self.terminal_cwds
            .retain(|pid, _| terminal_pids.contains(pid));

        for (terminal, detection, ssh_connection) in terminals {
            let id = format!(
                "{}/terminal/{}/{}",
                self.host,
                terminal_slug(&terminal.name),
                terminal.process_group
            );
            let observed_start =
                observed_process_start(&terminal.pids, &processes.process_started_at_ms, now);
            remember_record_start(&mut self.record_starts, &id, &id, observed_start);
            let old = self.previous.get(&id);
            let seen = next_seen(old, detection.state, false);
            let attention = attention(detection.state, seen);
            let goal = goal_lifecycle(old, detection.state, detection.goal, now);
            let changed_at_ms = old
                .filter(|old| old.state == detection.state && old.attention == attention)
                .map(|old| old.changed_at_ms)
                .unwrap_or(now);
            let cwd = self
                .terminal_cwds
                .get(&terminal.leader_pid)
                .cloned()
                .unwrap_or_default();
            let title = Path::new(&cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("terminal session")
                .to_string();
            let subagent_name = derived_subagent_name(&terminal.processes, &detection.agent);
            let codex_thread_id = detection
                .agent
                .eq_ignore_ascii_case("codex")
                .then(|| codex::resume_thread_id_from_processes(&terminal.processes))
                .flatten();
            let record = AgentRecord {
                id: id.clone(),
                host: self.host.clone(),
                server: self.server.clone(),
                pane_id: String::new(),
                pane_pid: terminal.leader_pid,
                session_id: String::new(),
                session_name: terminal.name.clone(),
                window_id: String::new(),
                window_index: 0,
                window_name: "terminal".into(),
                pane_index: 0,
                agent: detection.agent,
                state: detection.state,
                attention,
                source: detection.source,
                title,
                label: None,
                cwd,
                visible: false,
                seen,
                changed_at_ms,
                origin: AgentOrigin::Terminal,
                terminal: Some(terminal.name),
                remote_alias: None,
                ssh_connection,
                session_connections: None,
                focus_target: None,
                goal,
                subagent: None,
                detection: detection.details,
            };
            record_pids.insert(id.clone(), terminal.pids.into_iter().collect());
            if let Some(name) = subagent_name {
                subagent_names.insert(id.clone(), name);
            }
            if let Some(thread_id) = codex_thread_id {
                record_thread_ids.insert(id.clone(), thread_id);
            }
            next.insert(id, record);
        }

        for wrapped in runner_states
            .iter()
            .filter(|state| !claimed_runners.contains(&state.run_id))
        {
            let mut detection = wrapped.as_detection();
            let id = format!("{}/run/{}", self.host, wrapped.run_id);
            let old = self.previous.get(&id);
            let identity = format!("{}:{}", detection.agent, wrapped.run_id);
            let observed_start = observed_process_start(
                &[wrapped.owner_pid, wrapped.child_pid],
                &processes.process_started_at_ms,
                now,
            );
            remember_record_start(&mut self.record_starts, &id, &identity, observed_start);
            detection = self
                .detection_state
                .stabilize(&id, &identity, detection, old, now);
            let seen = next_seen(old, detection.state, false);
            let attention = attention(detection.state, seen);
            let goal = goal_lifecycle(old, detection.state, detection.goal, now);
            let changed_at_ms = old
                .filter(|old| old.state == detection.state && old.attention == attention)
                .map(|old| old.changed_at_ms)
                .unwrap_or(now);
            let title = if wrapped.title.is_empty() {
                Path::new(&wrapped.cwd)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("agent session")
                    .to_string()
            } else {
                wrapped.title.clone()
            };
            let codex_thread_id = wrapped
                .agent
                .eq_ignore_ascii_case("codex")
                .then(|| runner_codex_thread_id(wrapped, &processes.process_args))
                .flatten();
            next.insert(
                id.clone(),
                AgentRecord {
                    id,
                    host: self.host.clone(),
                    server: self.server.clone(),
                    pane_id: String::new(),
                    pane_pid: wrapped.owner_pid,
                    session_id: wrapped.run_id.clone(),
                    session_name: wrapped.run_id.clone(),
                    window_id: String::new(),
                    window_index: 0,
                    window_name: "runner".into(),
                    pane_index: 0,
                    agent: detection.agent,
                    state: detection.state,
                    attention,
                    source: detection.source,
                    title,
                    label: None,
                    cwd: wrapped.cwd.clone(),
                    visible: false,
                    seen,
                    changed_at_ms,
                    origin: AgentOrigin::Terminal,
                    terminal: wrapped.outer_terminal.clone(),
                    remote_alias: None,
                    ssh_connection: processes
                        .ssh_connections
                        .get(&wrapped.owner_pid)
                        .or_else(|| processes.ssh_connections.get(&wrapped.child_pid))
                        .cloned(),
                    session_connections: None,
                    focus_target: None,
                    goal,
                    subagent: None,
                    detection: detection.details,
                },
            );
            record_pids.insert(
                format!("{}/run/{}", self.host, wrapped.run_id),
                [wrapped.owner_pid, wrapped.child_pid].into_iter().collect(),
            );
            if let Some(thread_id) = codex_thread_id {
                record_thread_ids
                    .insert(format!("{}/run/{}", self.host, wrapped.run_id), thread_id);
            }
        }

        let thread_rollouts = self.codex_threads.scan(now, SUBAGENT_RETENTION_MS);
        let root_rollouts = self.codex_threads.root_rollouts().clone();
        self.codex_ownership
            .suppress_finished_processes_before_linking(
                &mut next,
                &mut record_pids,
                &thread_rollouts,
            );

        link_subagents(
            &mut next,
            &record_pids,
            &processes.parent_pids,
            &subagent_names,
            &self.previous,
            &self.record_starts,
            now,
        );
        let recovered_root_threads = collect_process_owned_root_thread_evidence(
            &next,
            &record_pids,
            &processes.process_args,
            &self.codex_threads,
            &record_thread_ids,
        );
        restore_previous_subagent_ancestry(&mut next, &self.previous, now);
        self.codex_ownership
            .reconcile_after_process_linking(ReconciliationFrame {
                records: &mut next,
                record_thread_ids: &mut record_thread_ids,
                record_starts: &self.record_starts,
                previous: &self.previous,
                threads: &thread_rollouts,
                root_rollouts: &root_rollouts,
                recovered_root_threads: &recovered_root_threads,
            });
        retain_finished_subagents(&mut next, &self.previous, now);
        for record in next.values_mut().filter(|record| record.is_tmux()) {
            // Retained children may outlive their session. Absence is authoritative
            // and must not reopen legacy marker recovery for a reused session name.
            record.session_connections = Some(
                session_connections
                    .get(&record.session_id)
                    .cloned()
                    .unwrap_or(crate::model::SessionConnections {
                        server_pid: 0,
                        server_started_at: 0,
                        session_created_at: 0,
                        complete: true,
                        clients: Vec::new(),
                    }),
            );
        }
        self.previous = next;
        self.record_starts
            .retain(|record_id, _| self.previous.contains_key(record_id));
        self.detection_state
            .retain(&self.previous.keys().cloned().collect());
        self.revision += 1;
        let mut snapshot = Snapshot {
            protocol: crate::model::PROTOCOL_VERSION,
            application_version: Some(crate::model::APPLICATION_VERSION.to_string()),
            capabilities: crate::model::application_capabilities(),
            revision: self.revision,
            host: self.host.clone(),
            server: self.server.clone(),
            generated_at_ms: now,
            agents: self.previous.values().cloned().collect(),
            peers: Vec::new(),
            ssh_transports: processes.ssh_transports,
        };
        snapshot.sort_agents();
        Ok(snapshot)
    }

    pub fn persisted(&self) -> PersistedState {
        PersistedState {
            protocol: crate::model::PROTOCOL_VERSION,
            host: self.host.clone(),
            server: self.server.clone(),
            agents: self.previous.values().cloned().collect(),
        }
    }
}

pub(crate) fn next_seen(old: Option<&AgentRecord>, state: AgentState, visible: bool) -> bool {
    if visible {
        return true;
    }
    let Some(old) = old else {
        return true;
    };
    match state {
        AgentState::Working => true,
        AgentState::Blocked => old.seen,
        AgentState::Idle => {
            if matches!(old.state, AgentState::Working | AgentState::Blocked) {
                false
            } else {
                old.seen
            }
        }
        AgentState::Unknown => old.seen,
    }
}

fn goal_lifecycle(
    old: Option<&AgentRecord>,
    state: AgentState,
    observed: Option<GoalInfo>,
    now_ms: u64,
) -> Option<GoalInfo> {
    let active = matches!(state, AgentState::Working | AgentState::Blocked);
    let was_active =
        old.is_some_and(|record| matches!(record.state, AgentState::Working | AgentState::Blocked));
    match observed {
        Some(mut goal) if goal.state == GoalState::Pursuing => {
            goal.achievement_pending = false;
            goal.achievement_observed_at_ms = 0;
            Some(goal)
        }
        Some(mut goal) => {
            let (pending, observed_at_ms, newly_achieved) = match old.and_then(|record| record.goal)
            {
                Some(previous) if previous.state == GoalState::Pursuing => (true, now_ms, true),
                Some(previous)
                    if previous.state == GoalState::Achieved
                        && previous.elapsed_seconds == goal.elapsed_seconds =>
                {
                    (
                        previous.achievement_pending,
                        previous.achievement_observed_at_ms,
                        false,
                    )
                }
                Some(previous) if previous.state == GoalState::Achieved => (true, now_ms, true),
                None if old.is_some_and(|record| {
                    matches!(record.state, AgentState::Working | AgentState::Blocked)
                }) =>
                {
                    (true, now_ms, true)
                }
                _ => (false, 0, false),
            };
            goal.achievement_pending = pending && (newly_achieved || !active || was_active);
            goal.achievement_observed_at_ms = observed_at_ms;
            Some(goal)
        }
        None => old.and_then(|record| record.goal).and_then(|mut goal| {
            (goal.state == GoalState::Achieved).then(|| {
                if active && !was_active {
                    goal.achievement_pending = false;
                }
                goal
            })
        }),
    }
}

pub(crate) fn attention(state: AgentState, seen: bool) -> Attention {
    match state {
        AgentState::Blocked => Attention::Blocked,
        AgentState::Working => Attention::Working,
        AgentState::Idle if !seen => Attention::Done,
        AgentState::Idle => Attention::Idle,
        AgentState::Unknown => Attention::Unknown,
    }
}

fn mark_process_only(detection: &mut detect::Detection) {
    detection.state = AgentState::Unknown;
    detection.source = EvidenceSource::Process;
    detection.goal = None;
    detection.details = None;
}

fn collected_title(wrapped: Option<&RunnerState>, agent: &str, pane_title: String) -> String {
    wrapped
        .filter(|state| !state.title.is_empty())
        .map(|state| state.title.clone())
        .or_else(|| detect::stable_title(agent, &pane_title))
        .unwrap_or(pane_title)
}

fn runner_for_pane<'a>(
    states: &'a [RunnerState],
    processes: &ProcessSnapshot,
    pane_id: &str,
) -> Option<&'a RunnerState> {
    let pane_pids = processes.pane_pids.get(pane_id)?;
    states
        .iter()
        .find(|state| pane_pids.contains(&state.owner_pid))
}

fn terminal_belongs_to_runner(terminal: &TerminalJob, states: &[RunnerState]) -> bool {
    states.iter().any(|state| {
        terminal.pids.contains(&state.owner_pid) || terminal.pids.contains(&state.child_pid)
    })
}

fn link_subagents(
    records: &mut HashMap<String, AgentRecord>,
    record_pids: &HashMap<String, HashSet<u32>>,
    parent_pids: &HashMap<u32, u32>,
    subagent_names: &HashMap<String, String>,
    previous: &HashMap<String, AgentRecord>,
    record_starts: &HashMap<String, (String, u64)>,
    now: u64,
) {
    let mut owners = HashMap::<u32, Vec<&str>>::new();
    for (record_id, pids) in record_pids {
        for pid in pids {
            owners.entry(*pid).or_default().push(record_id);
        }
    }
    let terminal_ids = records
        .values()
        .filter(|record| record.origin == AgentOrigin::Terminal)
        .map(|record| (record.id.clone(), record.pane_pid))
        .collect::<Vec<_>>();
    for (record_id, leader_pid) in terminal_ids {
        let parent_id = find_parent_agent(&record_id, leader_pid, parent_pids, &owners);
        let old = previous
            .get(&record_id)
            .and_then(|record| record.subagent.as_ref());
        let Some(parent_id) = parent_id.or_else(|| {
            old.filter(|subagent| subagent.finished_at_ms.is_none())
                .map(|subagent| subagent.parent_id.clone())
        }) else {
            continue;
        };
        let started_at_ms = old
            .filter(|subagent| subagent.parent_id == parent_id)
            .map(|subagent| subagent.started_at_ms)
            .or_else(|| record_starts.get(&record_id).map(|(_, started)| *started))
            .unwrap_or(now);
        let name = subagent_names
            .get(&record_id)
            .cloned()
            .or_else(|| old.and_then(|subagent| subagent.name.clone()));
        if let Some(record) = records.get_mut(&record_id) {
            record.subagent = Some(SubagentInfo {
                parent_id,
                started_at_ms,
                finished_at_ms: None,
                name,
                thread_id: old.and_then(|subagent| subagent.thread_id.clone()),
            });
        }
    }
}

fn find_parent_agent(
    record_id: &str,
    leader_pid: u32,
    parent_pids: &HashMap<u32, u32>,
    owners: &HashMap<u32, Vec<&str>>,
) -> Option<String> {
    let mut pid = parent_pids.get(&leader_pid).copied()?;
    for _ in 0..64 {
        if let Some(record_ids) = owners.get(&pid)
            && let Some(parent_id) = record_ids
                .iter()
                .copied()
                .find(|parent_id| *parent_id != record_id)
        {
            return Some(parent_id.to_string());
        }
        let parent = parent_pids.get(&pid).copied()?;
        if parent == 0 || parent == pid {
            return None;
        }
        pid = parent;
    }
    None
}

fn restore_previous_subagent_ancestry(
    records: &mut HashMap<String, AgentRecord>,
    previous: &HashMap<String, AgentRecord>,
    now: u64,
) {
    let relationships = records
        .iter()
        .filter_map(|(record_id, current)| {
            let current_subagent = current.subagent.as_ref()?;
            let old = previous.get(record_id)?;
            let old_subagent = old.subagent.as_ref()?;
            (old.pane_pid == current.pane_pid
                && old.agent == current.agent
                && old_subagent.parent_id != current_subagent.parent_id
                && previous
                    .get(&old_subagent.parent_id)
                    .is_some_and(|parent| parent.subagent.is_some()))
            .then(|| (record_id.clone(), old_subagent.parent_id.clone()))
        })
        .collect::<Vec<_>>();

    for (record_id, parent_id) in relationships {
        let mut required = vec![parent_id.clone()];
        let mut visited = HashSet::new();
        while let Some(ancestor_id) = required.pop() {
            if !visited.insert(ancestor_id.clone()) {
                break;
            }
            if let Some(ancestor) = records.get(&ancestor_id) {
                if let Some(subagent) = &ancestor.subagent {
                    required.push(subagent.parent_id.clone());
                }
                continue;
            }
            let Some(old) = previous.get(&ancestor_id) else {
                break;
            };
            let Some(subagent) = &old.subagent else {
                break;
            };
            let finished_at_ms = subagent.finished_at_ms.unwrap_or(now);
            let mut retained = old.clone();
            retained.state = AgentState::Idle;
            retained.attention = Attention::Done;
            retained.seen = false;
            retained.changed_at_ms = finished_at_ms;
            retained.subagent = Some(SubagentInfo {
                parent_id: subagent.parent_id.clone(),
                started_at_ms: subagent.started_at_ms,
                finished_at_ms: Some(finished_at_ms),
                name: subagent.name.clone(),
                thread_id: subagent.thread_id.clone(),
            });
            records.insert(ancestor_id, retained);
            required.push(subagent.parent_id.clone());
        }
        if records.contains_key(&parent_id)
            && let Some(subagent) = records
                .get_mut(&record_id)
                .and_then(|record| record.subagent.as_mut())
        {
            subagent.parent_id = parent_id;
        }
    }
}

fn runner_codex_thread_id(
    runner: &RunnerState,
    process_args: &HashMap<u32, String>,
) -> Option<String> {
    runner.codex_thread_id.clone().or_else(|| {
        [runner.child_pid, runner.owner_pid]
            .into_iter()
            .filter_map(|pid| process_args.get(&pid))
            .find_map(|args| codex::resume_thread_id_from_processes(args))
    })
}

fn pane_record_pids(
    pane_pid: u32,
    pane_pids: Option<&[u32]>,
    wrapped: Option<&RunnerState>,
) -> HashSet<u32> {
    let mut pids = pane_pids
        .map(|pids| pids.iter().copied().collect())
        .unwrap_or_else(|| HashSet::from([pane_pid]));
    if let Some(wrapped) = wrapped {
        pids.extend([wrapped.owner_pid, wrapped.child_pid]);
    }
    pids
}

fn collect_process_owned_root_thread_evidence(
    records: &HashMap<String, AgentRecord>,
    record_pids: &HashMap<String, HashSet<u32>>,
    process_args: &HashMap<u32, String>,
    tracker: &ThreadTracker,
    record_thread_ids: &HashMap<String, String>,
) -> HashMap<String, String> {
    let candidates = records
        .values()
        .filter(|record| {
            record.agent.eq_ignore_ascii_case("codex")
                && record.subagent.is_none()
                && !record_thread_ids.contains_key(&record.id)
                && record_pids.get(&record.id).is_some_and(|pids| {
                    pids.iter()
                        .filter_map(|pid| process_args.get(pid))
                        .any(|args| codex::codex_program_from_processes(args))
                })
        })
        .map(|record| record.id.clone())
        .collect::<Vec<_>>();
    let mut candidate_pids = candidates
        .iter()
        .filter_map(|record_id| record_pids.get(record_id))
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    candidate_pids.sort_unstable();
    candidate_pids.dedup();
    let Ok(files) = codex::process_rollout_files(&candidate_pids) else {
        return HashMap::new();
    };
    collect_process_owned_root_thread_evidence_from_files(candidates, record_pids, tracker, &files)
}

fn collect_process_owned_root_thread_evidence_from_files(
    candidates: Vec<String>,
    record_pids: &HashMap<String, HashSet<u32>>,
    tracker: &ThreadTracker,
    files: &HashMap<u32, Vec<PathBuf>>,
) -> HashMap<String, String> {
    let mut recovered = HashMap::new();
    for record_id in candidates {
        let Some(pids) = record_pids.get(&record_id) else {
            continue;
        };
        let mut pids = pids.iter().copied().collect::<Vec<_>>();
        pids.sort_unstable();
        let paths = pids
            .iter()
            .filter_map(|pid| files.get(pid))
            .flatten()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let Ok(Some(thread_id)) = tracker.root_thread_id_from_process_rollouts(paths) else {
            continue;
        };
        recovered.insert(record_id, thread_id);
    }
    recovered
}

fn remember_record_start(
    record_starts: &mut HashMap<String, (String, u64)>,
    record_id: &str,
    identity: &str,
    now: u64,
) {
    match record_starts.get_mut(record_id) {
        Some((current_identity, _)) if current_identity == identity => {}
        Some(entry) => *entry = (identity.to_string(), now),
        None => {
            record_starts.insert(record_id.to_string(), (identity.to_string(), now));
        }
    }
}

fn observed_process_start(
    pids: &[u32],
    process_started_at_ms: &HashMap<u32, u64>,
    fallback: u64,
) -> u64 {
    pids.iter()
        .filter_map(|pid| process_started_at_ms.get(pid).copied())
        .min()
        .unwrap_or(fallback)
}

fn retain_finished_subagents(
    next: &mut HashMap<String, AgentRecord>,
    previous: &HashMap<String, AgentRecord>,
    now: u64,
) {
    let current_thread_ids = next
        .values()
        .filter_map(|record| {
            record
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.clone())
        })
        .collect::<HashSet<_>>();
    for (id, old) in previous {
        if next.contains_key(id) {
            continue;
        }
        let Some(subagent) = &old.subagent else {
            continue;
        };
        if subagent
            .thread_id
            .as_deref()
            .is_some_and(|thread_id| current_thread_ids.contains(thread_id))
        {
            continue;
        }
        let finished_at_ms = subagent.finished_at_ms.unwrap_or(now);
        if now.saturating_sub(finished_at_ms) >= SUBAGENT_RETENTION_MS {
            continue;
        }
        let mut finished = old.clone();
        finished.state = AgentState::Idle;
        finished.attention = Attention::Done;
        finished.seen = false;
        finished.changed_at_ms = finished_at_ms;
        finished.subagent = Some(SubagentInfo {
            parent_id: subagent.parent_id.clone(),
            started_at_ms: subagent.started_at_ms,
            finished_at_ms: Some(finished_at_ms),
            name: subagent.name.clone(),
            thread_id: subagent.thread_id.clone(),
        });
        next.insert(id.clone(), finished);
    }
}

fn derived_subagent_name(processes: &str, agent: &str) -> Option<String> {
    let known_names: &[&str] = match agent {
        "Codex" => &["review", "exec", "apply", "cloud", "sandbox"],
        "Claude" => &["agent", "print"],
        "OpenCode" => &["run", "agent"],
        "Grok" => &[],
        "Pi" => &[],
        _ => &[],
    };
    processes.lines().find_map(|process| {
        process
            .split_whitespace()
            .map(|field| {
                field
                    .trim_matches(|character: char| {
                        !character.is_ascii_alphanumeric() && character != '-'
                    })
                    .trim_start_matches('-')
                    .to_ascii_lowercase()
            })
            .find(|field| known_names.contains(&field.as_str()))
    })
}

fn preserve_on_capture_failure(detection: &mut detect::Detection) {
    let direct_active_title = detection.source == EvidenceSource::Title
        && matches!(detection.state, AgentState::Working | AgentState::Blocked)
        && detection
            .details
            .as_ref()
            .is_some_and(|details| details.definitive);
    if direct_active_title {
        return;
    }
    if let Some(details) = &mut detection.details {
        details.preserve_previous = true;
        details.transition = Some("screen_capture_failed".into());
    }
}

fn terminal_slug(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn discover_host() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{RootRollout, ThreadRollout};
    use std::fs;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn capture_candidate<'a>(
        pane_id: &'a str,
        process_group: u32,
        foreground: bool,
    ) -> CaptureCandidate<'a> {
        CaptureCandidate {
            pane_id,
            identity: CaptureIdentity {
                pane_pid: 10,
                process_group,
                process_started_at_ms: Some(1_000),
            },
            foreground,
        }
    }

    #[test]
    fn foreground_is_due_every_pass_while_background_waits_one_second() {
        let started_at = Instant::now();
        let mut captures = CaptureCache::default();
        let foreground = capture_candidate("%1", 20, true);
        let background = capture_candidate("%2", 30, false);

        let due = captures.due_panes([foreground, background], started_at);
        assert_eq!(due, ["%1", "%2"]);
        captures.apply_results(
            &due,
            &HashMap::from([
                ("%1".into(), Ok("foreground screen".into())),
                ("%2".into(), Ok("background screen".into())),
            ]),
        );

        assert_eq!(
            captures.due_panes(
                [foreground, background],
                started_at + Duration::from_millis(999)
            ),
            ["%1"]
        );
        assert_eq!(captures.screen("%2"), Some("background screen"));
        assert_eq!(
            captures.due_panes(
                [foreground, background],
                started_at + Duration::from_secs(1)
            ),
            ["%1", "%2"]
        );
    }

    #[test]
    fn replaced_process_identity_is_captured_without_reusing_its_screen() {
        let started_at = Instant::now();
        let mut captures = CaptureCache::default();
        let original = capture_candidate("%1", 20, false);
        let due = captures.due_panes([original], started_at);
        assert_eq!(due, ["%1"]);
        captures.apply_results(
            &due,
            &HashMap::from([("%1".into(), Ok("original screen".into()))]),
        );

        let mut replacement = original;
        replacement.identity.process_started_at_ms = Some(2_000);
        assert_eq!(
            captures.due_panes([replacement], started_at + Duration::from_millis(100)),
            ["%1"]
        );
        assert_eq!(captures.screen("%1"), None);
    }

    #[test]
    fn panes_that_stop_being_capture_candidates_are_pruned() {
        let started_at = Instant::now();
        let mut captures = CaptureCache::default();
        let pane = capture_candidate("%1", 20, false);
        let due = captures.due_panes([pane], started_at);
        captures.apply_results(&due, &HashMap::from([("%1".into(), Ok("screen".into()))]));

        assert!(captures.due_panes([], started_at).is_empty());
        assert_eq!(captures.screen("%1"), None);
    }

    #[test]
    fn failed_background_capture_drops_stale_screen_and_retries_after_one_second() {
        let started_at = Instant::now();
        let mut captures = CaptureCache::default();
        let pane = capture_candidate("%1", 20, false);
        let due = captures.due_panes([pane], started_at);
        captures.apply_results(&due, &HashMap::from([("%1".into(), Ok("screen".into()))]));

        let due = captures.due_panes([pane], started_at + Duration::from_secs(1));
        assert_eq!(due, ["%1"]);
        captures.apply_results(
            &due,
            &HashMap::from([("%1".into(), Err(anyhow::anyhow!("capture failed")))]),
        );

        assert_eq!(captures.screen("%1"), None);
        assert!(
            captures
                .due_panes([pane], started_at + Duration::from_millis(1_999))
                .is_empty()
        );
        assert_eq!(
            captures.due_panes([pane], started_at + Duration::from_secs(2)),
            ["%1"]
        );
    }

    #[test]
    fn cached_quiet_screen_does_not_count_as_a_second_idle_observation() {
        let started_at = Instant::now();
        let pane = capture_candidate("%1", 20, false);
        let mut captures = CaptureCache::default();
        let mut tracker = StateTracker::default();
        let previous = old(AgentState::Working, true);

        let first_due = captures.due_panes([pane], started_at);
        captures.apply_results(&first_due, &HashMap::from([("%1".into(), Ok("".into()))]));
        let first = tracker.stabilize_observation(
            &previous.id,
            "Codex:20",
            detect::detect("codex", "", captures.screen("%1").unwrap()).unwrap(),
            Some(&previous),
            1_000,
            ObservationFreshness::Fresh,
        );
        assert_eq!(first.state, AgentState::Working);

        let cached_due = captures.due_panes([pane], started_at + Duration::from_millis(300));
        assert!(cached_due.is_empty());
        let cached = tracker.stabilize_observation(
            &previous.id,
            "Codex:20",
            detect::detect("codex", "", captures.screen("%1").unwrap()).unwrap(),
            Some(&previous),
            1_300,
            ObservationFreshness::Replayed,
        );
        assert_eq!(cached.state, AgentState::Working);

        let second_due = captures.due_panes([pane], started_at + Duration::from_secs(1));
        captures.apply_results(&second_due, &HashMap::from([("%1".into(), Ok("".into()))]));
        let second = tracker.stabilize_observation(
            &previous.id,
            "Codex:20",
            detect::detect("codex", "", captures.screen("%1").unwrap()).unwrap(),
            Some(&previous),
            2_000,
            ObservationFreshness::Fresh,
        );
        assert_eq!(second.state, AgentState::Idle);
    }

    #[test]
    fn claude_completed_turn_stays_idle_while_background_shell_persists() {
        let screen = "Done.\n✻ Worked for 46m · done · 1 shell still running\n────\n❯ editable unsent text\n────\nmodel · project · main · Context 23% left\n⏵⏵ auto mode on · 1 shell · ← 1 agent";
        let mut tracker = StateTracker::default();
        let mut previous = old(AgentState::Idle, true);
        previous.agent = "Claude".into();

        for (now, title, freshness, expected) in [
            (
                1_000,
                "◐ task",
                ObservationFreshness::Fresh,
                AgentState::Working,
            ),
            (2_000, "", ObservationFreshness::Fresh, AgentState::Idle),
            (2_300, "", ObservationFreshness::Replayed, AgentState::Idle),
            (3_000, "", ObservationFreshness::Fresh, AgentState::Idle),
        ] {
            let result = tracker.stabilize_observation(
                &previous.id,
                "Claude:20",
                detect::detect("claude", title, screen).unwrap(),
                Some(&previous),
                now,
                freshness,
            );
            assert_eq!(result.state, expected, "observation at {now}");
            if expected == AgentState::Idle {
                let details = result.details.as_ref().unwrap();
                assert_eq!(details.signal.as_deref(), Some("input_prompt"));
                assert!(details.definitive);
                assert!(!details.inferred);
            }
            previous.state = result.state;
            previous.source = result.source;
            previous.detection = result.details;
        }
    }

    #[test]
    fn cached_screen_still_applies_current_title_evidence() {
        let previous = old(AgentState::Working, true);
        let mut tracker = StateTracker::default();
        let detection = detect::detect("codex", "Action Required", "").unwrap();

        let result = tracker.stabilize_observation(
            &previous.id,
            "Codex:20",
            detection,
            Some(&previous),
            1_300,
            ObservationFreshness::Replayed,
        );

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.source, EvidenceSource::Title);
    }

    fn old(state: AgentState, seen: bool) -> AgentRecord {
        AgentRecord {
            id: "host/default/%1".into(),
            host: "host".into(),
            server: "default".into(),
            pane_id: "%1".into(),
            pane_pid: 1,
            session_id: "$1".into(),
            session_name: "main".into(),
            window_id: "@1".into(),
            window_index: 1,
            window_name: "work".into(),
            pane_index: 0,
            agent: "Codex".into(),
            state,
            attention: attention(state, seen),
            source: crate::model::EvidenceSource::Screen,
            title: String::new(),
            label: None,
            cwd: String::new(),
            visible: false,
            seen,
            changed_at_ms: 1,
            origin: AgentOrigin::Tmux,
            terminal: None,
            remote_alias: None,
            ssh_connection: None,
            session_connections: None,
            focus_target: None,
            goal: None,
            subagent: None,
            detection: None,
        }
    }

    fn pursuing_goal(elapsed_seconds: u64) -> GoalInfo {
        GoalInfo {
            state: GoalState::Pursuing,
            elapsed_seconds,
            achievement_pending: false,
            achievement_observed_at_ms: 0,
        }
    }

    fn achieved_goal(elapsed_seconds: u64, achievement_pending: bool) -> GoalInfo {
        GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds,
            achievement_pending,
            achievement_observed_at_ms: if achievement_pending { 1_000 } else { 0 },
        }
    }

    fn wrapped() -> RunnerState {
        RunnerState {
            protocol: crate::runner::RUNNER_PROTOCOL,
            run_id: "run-1".into(),
            owner_pid: 20,
            child_pid: 30,
            process_group: 30,
            agent: "Codex".into(),
            state: AgentState::Working,
            source: EvidenceSource::Screen,
            cwd: "/work".into(),
            title: "⠸ work".into(),
            outer_terminal: Some("ttys001".into()),
            inner_terminal: Some("ttys002".into()),
            updated_at_ms: 1,
            codex_thread_id: None,
            goal: None,
            detection: None,
        }
    }

    fn process_snapshot() -> ProcessSnapshot {
        ProcessSnapshot {
            panes: HashMap::new(),
            pane_groups: HashMap::new(),
            pane_pids: HashMap::from([("%1".into(), vec![20])]),
            process_args: HashMap::new(),
            process_started_at_ms: HashMap::new(),
            terminals: Vec::new(),
            live_pids: [20, 30, 31].into_iter().collect(),
            parent_pids: HashMap::new(),
            ssh_connections: HashMap::new(),
            client_connections: HashMap::new(),
            ssh_transports: Vec::new(),
        }
    }

    #[test]
    fn legacy_runner_recovers_codex_thread_id_from_owner_command() {
        let id = "01800000-0000-7000-8000-000000000001";
        let runner = wrapped();
        let mut processes = process_snapshot();
        processes.process_args = HashMap::from([
            (
                runner.child_pid,
                format!(
                    "node /opt/homebrew/bin/codex --config \
                     developer_instructions=\"Effective actor: Example User\" resume {id}"
                ),
            ),
            (
                runner.owner_pid,
                format!("tmux-agent run -- codex resume {id}"),
            ),
        ]);

        assert!(
            codex::resume_thread_id_from_processes(&processes.process_args[&runner.child_pid])
                .is_none()
        );
        assert_eq!(
            runner_codex_thread_id(&runner, &processes.process_args).as_deref(),
            Some(id)
        );
    }

    #[test]
    fn background_completion_becomes_unseen_done() {
        let old = old(AgentState::Working, true);
        let seen = next_seen(Some(&old), AgentState::Idle, false);
        assert!(!seen);
        assert_eq!(attention(AgentState::Idle, seen), Attention::Done);
    }

    #[test]
    fn visible_completion_is_already_seen() {
        let old = old(AgentState::Working, true);
        let seen = next_seen(Some(&old), AgentState::Idle, true);
        assert!(seen);
        assert_eq!(attention(AgentState::Idle, seen), Attention::Idle);
    }

    #[test]
    fn pursuing_to_achieved_creates_a_pending_notice() {
        let mut previous = old(AgentState::Working, true);
        previous.goal = Some(pursuing_goal(1_122));

        let goal = goal_lifecycle(
            Some(&previous),
            AgentState::Idle,
            Some(achieved_goal(1_122, false)),
            2_000,
        )
        .unwrap();

        assert_eq!(goal.state, GoalState::Achieved);
        assert!(goal.achievement_pending);
        assert_eq!(goal.achievement_observed_at_ms, 2_000);
    }

    #[test]
    fn first_observed_historical_achievement_is_not_pending() {
        let goal = goal_lifecycle(
            None,
            AgentState::Idle,
            Some(achieved_goal(7_920, false)),
            2_000,
        )
        .unwrap();

        assert!(!goal.achievement_pending);
    }

    #[test]
    fn pending_achievement_does_not_expire_without_a_transition() {
        let mut previous = old(AgentState::Idle, true);
        previous.goal = Some(achieved_goal(7_920, true));

        let goal = goal_lifecycle(
            Some(&previous),
            AgentState::Idle,
            Some(achieved_goal(7_920, false)),
            999_999_999,
        )
        .unwrap();

        assert!(goal.achievement_pending);
    }

    #[test]
    fn achievement_observed_before_active_turn_finishes_is_deferred_until_idle() {
        let mut working = old(AgentState::Working, true);
        working.goal = Some(pursuing_goal(7_920));

        let achieved_while_working = goal_lifecycle(
            Some(&working),
            AgentState::Working,
            Some(achieved_goal(7_920, false)),
            2_000,
        )
        .unwrap();
        assert!(achieved_while_working.achievement_pending);
        assert_eq!(achieved_while_working.achievement_observed_at_ms, 2_000);

        working.goal = Some(achieved_while_working);
        let achieved_while_idle = goal_lifecycle(
            Some(&working),
            AgentState::Idle,
            Some(achieved_goal(7_920, false)),
            3_000,
        )
        .unwrap();

        assert!(achieved_while_idle.achievement_pending);
        assert_eq!(achieved_while_idle.achievement_observed_at_ms, 2_000);
    }

    #[test]
    fn missing_footer_does_not_discard_a_pending_achievement() {
        let mut previous = old(AgentState::Idle, true);
        previous.goal = Some(achieved_goal(7_920, true));

        let goal = goal_lifecycle(Some(&previous), AgentState::Idle, None, 999_999_999).unwrap();

        assert_eq!(goal, achieved_goal(7_920, true));
    }

    #[test]
    fn active_turn_retires_the_previous_achievement() {
        let mut previous = old(AgentState::Idle, true);
        previous.goal = Some(achieved_goal(7_920, true));

        let goal = goal_lifecycle(
            Some(&previous),
            AgentState::Working,
            Some(achieved_goal(7_920, false)),
            2_000,
        )
        .unwrap();

        assert!(!goal.achievement_pending);
    }

    #[test]
    fn new_goal_cycle_creates_a_fresh_pending_achievement() {
        let mut previous = old(AgentState::Idle, true);
        previous.goal = Some(achieved_goal(7_920, false));
        let pursuing = goal_lifecycle(
            Some(&previous),
            AgentState::Working,
            Some(pursuing_goal(5)),
            2_000,
        )
        .unwrap();
        previous.state = AgentState::Working;
        previous.goal = Some(pursuing);

        let achieved = goal_lifecycle(
            Some(&previous),
            AgentState::Idle,
            Some(achieved_goal(42, false)),
            3_000,
        )
        .unwrap();

        assert!(achieved.achievement_pending);
        assert_eq!(achieved.elapsed_seconds, 42);
    }

    #[test]
    fn terminal_names_are_safe_in_agent_ids() {
        assert_eq!(terminal_slug("/dev/ttys003"), "_dev_ttys003");
    }

    #[test]
    fn ordinary_terminal_without_owned_screen_is_unknown() {
        for provider in ["/opt/homebrew/bin/codex", "/opt/homebrew/bin/omp"] {
            let mut detection = detect::detect(provider, "", "").unwrap();
            mark_process_only(&mut detection);
            assert_eq!(detection.state, AgentState::Unknown);
            assert_eq!(detection.source, EvidenceSource::Process);
            assert!(detection.details.is_none());
        }
    }

    #[test]
    fn omp_spinner_frames_produce_the_same_collected_record_title() {
        assert_eq!(
            collected_title(None, "OMP", "π ⠋ local-bench".into()),
            "local-bench"
        );
        assert_eq!(
            collected_title(None, "OMP", "π ⠸ local-bench".into()),
            "local-bench"
        );
        assert_eq!(
            collected_title(None, "Codex", "⠸ local-bench".into()),
            "⠸ local-bench"
        );
    }

    #[test]
    fn runner_in_tmux_is_joined_to_its_outer_pane() {
        let states = [wrapped()];
        let processes = process_snapshot();
        assert_eq!(
            runner_for_pane(&states, &processes, "%1").map(|state| state.run_id.as_str()),
            Some("run-1")
        );
    }

    #[test]
    fn claimed_tmux_runner_owns_its_inner_codex_child_for_rollout_discovery() {
        let runner = wrapped();

        assert_eq!(
            pane_record_pids(10, Some(&[10, runner.owner_pid]), Some(&runner)),
            HashSet::from([10, runner.owner_pid, runner.child_pid])
        );
    }

    #[test]
    fn runner_suppresses_outer_and_inner_terminal_duplicates() {
        let runner = wrapped();
        let outer = TerminalJob {
            name: "ttys001".into(),
            process_group: 20,
            leader_pid: 20,
            pids: vec![20],
            processes: "tmux-agent run -- codex".into(),
        };
        let inner = TerminalJob {
            name: "ttys002".into(),
            process_group: 30,
            leader_pid: 30,
            pids: vec![30, 31],
            processes: "codex".into(),
        };
        assert!(terminal_belongs_to_runner(
            &outer,
            std::slice::from_ref(&runner)
        ));
        assert!(terminal_belongs_to_runner(&inner, &[runner]));
    }

    #[test]
    fn capture_failure_preserves_state_without_strong_title_evidence() {
        let mut detection = detect::detect("codex", "work", "").unwrap();
        preserve_on_capture_failure(&mut detection);
        assert!(detection.details.as_ref().unwrap().preserve_previous);
        let mut tracker = StateTracker::default();
        let previous = old(AgentState::Working, true);
        let result = tracker.stabilize(&previous.id, "Codex:42", detection, Some(&previous), 4_000);
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(
            result.details.unwrap().transition.as_deref(),
            Some("screen_capture_failed")
        );
    }

    #[test]
    fn capture_failure_keeps_strong_title_evidence() {
        let mut detection = detect::detect("codex", "Action Required", "").unwrap();
        preserve_on_capture_failure(&mut detection);
        let details = detection.details.unwrap();
        assert!(!details.preserve_previous);
        assert_eq!(details.signal.as_deref(), Some("title_requests_action"));
    }

    #[test]
    fn descendant_terminal_is_linked_to_nearest_agent_parent() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.pane_pid = 20;
        parent.origin = AgentOrigin::Terminal;
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.pane_pid = 70;
        child.origin = AgentOrigin::Terminal;
        let mut records = HashMap::from([(parent.id.clone(), parent), (child.id.clone(), child)]);
        let record_pids = HashMap::from([
            ("host/run/main".into(), HashSet::from([20, 30])),
            ("host/terminal/ttys002/70".into(), HashSet::from([70, 71])),
        ]);
        let parent_pids = HashMap::from([(70, 60), (60, 50), (50, 30), (30, 20), (20, 1)]);
        let subagent_names = HashMap::from([("host/terminal/ttys002/70".into(), "review".into())]);

        link_subagents(
            &mut records,
            &record_pids,
            &parent_pids,
            &subagent_names,
            &HashMap::new(),
            &HashMap::new(),
            5_000,
        );

        assert_eq!(
            records["host/terminal/ttys002/70"].subagent,
            Some(SubagentInfo {
                parent_id: "host/run/main".into(),
                started_at_ms: 5_000,
                finished_at_ms: None,
                name: Some("review".into()),
                thread_id: None,
            })
        );
        assert!(records["host/run/main"].subagent.is_none());
    }

    #[test]
    fn subagent_completion_is_retained_for_thirty_seconds() {
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: "host/run/main".into(),
            started_at_ms: 1_000,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut previous = HashMap::from([(child.id.clone(), child)]);
        let mut next = HashMap::new();

        retain_finished_subagents(&mut next, &previous, 11_000);
        let finished = next["host/terminal/ttys002/70"].clone();
        assert_eq!(finished.attention, Attention::Done);
        assert_eq!(
            finished
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.finished_at_ms),
            Some(11_000)
        );

        previous = next;
        let mut before_expiry = HashMap::new();
        retain_finished_subagents(&mut before_expiry, &previous, 40_999);
        assert!(before_expiry.contains_key("host/terminal/ttys002/70"));

        let mut at_expiry = HashMap::new();
        retain_finished_subagents(&mut at_expiry, &previous, 41_000);
        assert!(!at_expiry.contains_key("host/terminal/ttys002/70"));
    }

    #[test]
    fn active_subagent_preserves_first_observed_start_time() {
        let mut old_child = old(AgentState::Unknown, true);
        old_child.id = "host/terminal/ttys002/70".into();
        old_child.origin = AgentOrigin::Terminal;
        old_child.subagent = Some(SubagentInfo {
            parent_id: "host/run/main".into(),
            started_at_ms: 1_000,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let previous = HashMap::from([(old_child.id.clone(), old_child)]);
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.pane_pid = 20;
        parent.origin = AgentOrigin::Terminal;
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.pane_pid = 70;
        child.origin = AgentOrigin::Terminal;
        let mut records = HashMap::from([(parent.id.clone(), parent), (child.id.clone(), child)]);
        let record_pids = HashMap::from([
            ("host/run/main".into(), HashSet::from([20])),
            ("host/terminal/ttys002/70".into(), HashSet::from([70])),
        ]);

        link_subagents(
            &mut records,
            &record_pids,
            &HashMap::from([(70, 20), (20, 1)]),
            &HashMap::new(),
            &previous,
            &HashMap::new(),
            9_000,
        );

        assert_eq!(
            records["host/terminal/ttys002/70"]
                .subagent
                .as_ref()
                .map(|subagent| subagent.started_at_ms),
            Some(1_000)
        );
        assert_eq!(
            records["host/terminal/ttys002/70"]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.name.as_deref()),
            Some("review")
        );
    }

    fn codex_thread(finished_at_ms: Option<u64>) -> ThreadRollout {
        ThreadRollout {
            thread_id: "01800000-0000-7000-8000-000000000002".into(),
            parent_thread_id: "01800000-0000-7000-8000-000000000001".into(),
            cwd: "/work".into(),
            started_at_ms: 5_000,
            finished_at_ms,
            name: Some("Worker".into()),
            agent_path: None,
            depth: Some(1),
            process_backed: true,
        }
    }

    fn reconcile_codex_ownership(
        records: &mut HashMap<String, AgentRecord>,
        threads: &[ThreadRollout],
        root_rollouts: &HashMap<String, RootRollout>,
        record_thread_ids: &HashMap<String, String>,
        record_starts: &HashMap<String, (String, u64)>,
        previous: &HashMap<String, AgentRecord>,
        recovered_root_threads: &HashMap<String, String>,
    ) {
        let mut ownership = CodexOwnership::new(previous, "host", "default");
        let mut record_thread_ids = record_thread_ids.clone();
        ownership.reconcile_after_process_linking(ReconciliationFrame {
            records,
            record_thread_ids: &mut record_thread_ids,
            record_starts,
            previous,
            threads,
            root_rollouts,
            recovered_root_threads,
        });
    }

    #[test]
    fn process_owned_roots_keep_same_cwd_children_with_their_exact_parent() {
        let directory = tempdir().unwrap();
        let day = directory.path().join("2026/07/26");
        fs::create_dir_all(&day).unwrap();
        let resumed_thread_id = "01800000-0000-7000-8000-000000000001";
        let unrelated_thread_id = "01800000-0000-7000-8000-000000000099";
        let resumed_rollout = day.join("rollout-resumed.jsonl");
        let unrelated_rollout = day.join("rollout-unrelated.jsonl");
        for (path, thread_id) in [
            (&resumed_rollout, resumed_thread_id),
            (&unrelated_rollout, unrelated_thread_id),
        ] {
            fs::write(
                path,
                format!(
                    "{}\n",
                    serde_json::json!({
                        "timestamp": "2026-07-26T14:24:55.000Z",
                        "type": "session_meta",
                        "payload": {
                            "id": thread_id,
                            "thread_source": "user",
                            "cwd": "/work",
                            "timestamp": "2026-07-26T14:24:55.000Z",
                            "source": "cli"
                        }
                    })
                ),
            )
            .unwrap();
        }
        let mut tracker = ThreadTracker::new(Some(directory.path().to_path_buf()));
        tracker.scan(1_785_033_103_000, 30_000);

        let mut resumed = old(AgentState::Working, true);
        resumed.id = "host/default/%1".into();
        resumed.cwd = "/different-launch-directory".into();
        let resumed_id = resumed.id.clone();
        let mut unrelated = old(AgentState::Working, true);
        unrelated.id = "host/default/%2".into();
        unrelated.cwd = "/work".into();
        let unrelated_id = unrelated.id.clone();
        let mut records = HashMap::from([
            (resumed_id.clone(), resumed),
            (unrelated_id.clone(), unrelated),
        ]);
        let record_pids = HashMap::from([
            (resumed_id.clone(), HashSet::from([101])),
            (unrelated_id.clone(), HashSet::from([202])),
        ]);
        let files = HashMap::from([(101, vec![resumed_rollout]), (202, vec![unrelated_rollout])]);
        let recovered = collect_process_owned_root_thread_evidence_from_files(
            vec![resumed_id.clone(), unrelated_id.clone()],
            &record_pids,
            &tracker,
            &files,
        );
        assert_eq!(recovered.len(), 2);

        reconcile_codex_ownership(
            &mut records,
            &[codex_thread(None)],
            tracker.root_rollouts(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &recovered,
        );

        let child = &records["host/codex-thread/01800000-0000-7000-8000-000000000002"];
        assert_eq!(
            child
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(resumed_id.as_str())
        );
        assert_ne!(
            child
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(unrelated_id.as_str())
        );
    }

    #[test]
    fn completed_thread_ancestor_is_retained_for_active_process_child() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut review = old(AgentState::Unknown, true);
        review.id = "host/terminal/ttys002/70".into();
        review.cwd = "/work".into();
        review.origin = AgentOrigin::Terminal;
        review.subagent = Some(SubagentInfo {
            parent_id: parent_id.clone(),
            started_at_ms: 5_500,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let review_id = review.id.clone();
        let mut thread = codex_thread(None);
        thread.agent_path = Some("/root/codex_review".into());
        thread.process_backed = false;
        let synthetic_id = "host/codex-thread/01800000-0000-7000-8000-000000000002".to_string();
        let mut previous = HashMap::from([(parent_id.clone(), parent.clone())]);
        reconcile_codex_ownership(
            &mut previous,
            &[thread],
            &HashMap::new(),
            &HashMap::from([(
                parent_id.clone(),
                "01800000-0000-7000-8000-000000000001".into(),
            )]),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        review.subagent.as_mut().unwrap().parent_id = synthetic_id.clone();
        previous.insert(review_id.clone(), review.clone());
        review.subagent.as_mut().unwrap().parent_id = parent_id.clone();
        let mut records = HashMap::from([(parent_id, parent), (review_id.clone(), review)]);

        restore_previous_subagent_ancestry(&mut records, &previous, 10_000);

        let retained = &records[&synthetic_id];
        assert_eq!(retained.attention, Attention::Done);
        assert_eq!(
            retained
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.finished_at_ms),
            Some(10_000)
        );
        assert_eq!(
            records[&review_id]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(synthetic_id.as_str())
        );
    }

    #[test]
    fn active_rollout_replaces_a_disappeared_process_child_without_done_duplicate() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut old_child = old(AgentState::Working, true);
        old_child.id = "host/terminal/ttys002/70".into();
        old_child.cwd = "/work".into();
        old_child.origin = AgentOrigin::Terminal;
        old_child.subagent = Some(SubagentInfo {
            parent_id: parent_id.clone(),
            started_at_ms: 5_000,
            finished_at_ms: None,
            name: Some("Worker".into()),
            thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
        });
        let previous = HashMap::from([(old_child.id.clone(), old_child)]);
        let mut records = HashMap::from([(parent_id.clone(), parent)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        reconcile_codex_ownership(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &previous,
            &HashMap::new(),
        );
        retain_finished_subagents(&mut records, &previous, 10_000);

        assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
        assert!(!records.contains_key("host/terminal/ttys002/70"));
    }

    #[test]
    fn subagent_name_is_derived_from_a_known_command_role_only() {
        let review = "node /opt/homebrew/bin/codex review --base develop\n/opt/bin/codex review";
        assert_eq!(
            derived_subagent_name(review, "Codex").as_deref(),
            Some("review")
        );
        assert_eq!(
            derived_subagent_name("/opt/homebrew/bin/codex resume session-id", "Codex"),
            None
        );
        assert_eq!(
            derived_subagent_name("/opt/bin/claude --print prompt", "Claude").as_deref(),
            Some("print")
        );
    }
}
