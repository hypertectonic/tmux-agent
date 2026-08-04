use crate::codex::{self, RootRollout, ThreadRollout, ThreadTracker};
use crate::config::Config;
use crate::detect;
use crate::detect::stabilize::StateTracker;
use crate::model::{
    AgentOrigin, AgentRecord, AgentState, Attention, EvidenceSource, GoalInfo, GoalState,
    PROTOCOL_VERSION, PersistedState, Snapshot, SubagentInfo,
};
use crate::runner::{self, RunnerState};
use crate::tmux::{ProcessSnapshot, TerminalJob, Tmux};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SUBAGENT_RETENTION_MS: u64 = 30_000;

pub struct Scanner {
    tmux: Tmux,
    host: String,
    server: String,
    previous: HashMap<String, AgentRecord>,
    detection_state: StateTracker,
    terminal_cwds: HashMap<u32, String>,
    runner_directory: PathBuf,
    codex_threads: ThreadTracker,
    finished_process_threads: HashMap<String, String>,
    record_starts: HashMap<String, (String, u64)>,
    revision: u64,
}

impl Scanner {
    pub fn new(
        config: &Config,
        tmux: Tmux,
        server_key: &str,
        runner_directory: PathBuf,
        persisted: Option<PersistedState>,
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
        let mut finished_process_threads = HashMap::new();
        remember_finished_process_threads(&previous, &host, &mut finished_process_threads);
        Ok(Self {
            tmux,
            host,
            server,
            previous,
            detection_state: StateTracker::default(),
            terminal_cwds: HashMap::new(),
            runner_directory,
            codex_threads: ThreadTracker::from_environment(),
            finished_process_threads,
            record_starts: HashMap::new(),
            revision: 0,
        })
    }

    pub fn scan(&mut self) -> Result<Snapshot> {
        let panes = self.tmux.list_panes()?;
        let processes = self.tmux.process_snapshot(&panes)?;
        let runner_states = runner::load_states(&self.runner_directory, &processes.live_pids);
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
            let mut detection = if let Some(wrapped) = wrapped {
                claimed_runners.insert(wrapped.run_id.clone());
                wrapped.as_detection()
            } else {
                let captured_screen = self.tmux.capture_visible(&pane.pane_id);
                let screen = captured_screen.as_deref().unwrap_or_default();
                let Some(mut detection) = detect::detect(process, &pane.title, screen) else {
                    continue;
                };
                if captured_screen.is_err() {
                    preserve_on_capture_failure(&mut detection);
                }
                detection
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
            detection = self
                .detection_state
                .stabilize(&id, &identity, detection, old, now);
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
                title: wrapped
                    .filter(|state| !state.title.is_empty())
                    .map(|state| state.title.clone())
                    .unwrap_or(pane.title),
                label: pane.label,
                cwd,
                visible: pane.visible,
                seen,
                changed_at_ms,
                origin: AgentOrigin::Tmux,
                terminal: None,
                remote_alias: None,
                ssh_connection: None,
                focus_target: None,
                goal,
                subagent: None,
                detection: detection.details,
            };
            record_pids.insert(
                id.clone(),
                processes
                    .pane_pids
                    .get(&record.pane_id)
                    .cloned()
                    .unwrap_or_else(|| vec![record.pane_pid])
                    .into_iter()
                    .collect(),
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
        suppress_finished_process_threads(
            &mut next,
            &mut record_pids,
            &mut self.finished_process_threads,
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
        restore_previous_subagent_ancestry(&mut next, &self.previous, now);
        link_codex_thread_subagents(
            &mut next,
            &thread_rollouts,
            &root_rollouts,
            &record_thread_ids,
            &self.record_starts,
            &self.previous,
            (&self.host, &self.server),
        );
        remember_finished_process_threads(&next, &self.host, &mut self.finished_process_threads);
        retain_finished_subagents(&mut next, &self.previous, now);
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

fn next_seen(old: Option<&AgentRecord>, state: AgentState, visible: bool) -> bool {
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

fn attention(state: AgentState, seen: bool) -> Attention {
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

fn link_codex_thread_subagents(
    records: &mut HashMap<String, AgentRecord>,
    threads: &[ThreadRollout],
    root_rollouts: &HashMap<String, RootRollout>,
    record_thread_ids: &HashMap<String, String>,
    record_starts: &HashMap<String, (String, u64)>,
    previous: &HashMap<String, AgentRecord>,
    location: (&str, &str),
) {
    let (host, server) = location;
    let explicit_parents = unambiguous_thread_parents(record_thread_ids);
    let thread_ids = threads
        .iter()
        .map(|thread| thread.thread_id.as_str())
        .collect::<HashSet<_>>();
    let mut linked_threads = records
        .values()
        .filter_map(|record| {
            record
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_ref())
                .map(|thread_id| (thread_id.clone(), record.id.clone()))
        })
        .collect::<HashMap<_, _>>();
    let mut pending = threads.iter().collect::<Vec<_>>();
    pending.sort_by_key(|thread| (thread.depth.unwrap_or(1), thread.started_at_ms));
    loop {
        let mut progress = false;
        let mut remaining = Vec::new();
        for thread in pending {
            let parent_id = explicit_parents
                .get(&thread.parent_thread_id)
                .or_else(|| linked_threads.get(&thread.parent_thread_id))
                .cloned()
                .or_else(|| {
                    (thread.depth.unwrap_or(1) <= 1
                        && !thread_ids.contains(thread.parent_thread_id.as_str()))
                    .then(|| {
                        root_rollouts
                            .get(&thread.parent_thread_id)
                            .and_then(|root| unique_root_parent(records, root, record_starts))
                            .or_else(|| unique_cwd_parent(records, thread, record_starts))
                    })
                    .flatten()
                });
            let Some(parent_id) = parent_id else {
                remaining.push(thread);
                continue;
            };
            if let Some(record_id) =
                attach_thread_to_process_child(records, threads, thread, &parent_id, previous)
            {
                reparent_agent_path_process_child(records, threads, thread, &parent_id, &record_id);
                linked_threads.insert(thread.thread_id.clone(), record_id);
                progress = true;
                continue;
            }
            let synthetic_id = format!("{host}/codex-thread/{}", thread.thread_id);
            if insert_synthetic_thread(records, thread, &parent_id, &synthetic_id, server) {
                reparent_agent_path_process_child(
                    records,
                    threads,
                    thread,
                    &parent_id,
                    &synthetic_id,
                );
                linked_threads.insert(thread.thread_id.clone(), synthetic_id);
                progress = true;
            } else {
                remaining.push(thread);
            }
        }
        if !progress || remaining.is_empty() {
            break;
        }
        pending = remaining;
    }
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

fn reparent_agent_path_process_child(
    records: &mut HashMap<String, AgentRecord>,
    threads: &[ThreadRollout],
    thread: &ThreadRollout,
    root_parent_id: &str,
    thread_record_id: &str,
) {
    if thread.process_backed {
        return;
    }
    let mut matches = records.values().filter(|record| {
        record.id != thread_record_id
            && record.agent.eq_ignore_ascii_case("codex")
            && same_cwd(&record.cwd, &thread.cwd)
            && record.subagent.as_ref().is_some_and(|subagent| {
                subagent.parent_id == root_parent_id
                    && subagent.thread_id.is_none()
                    && subagent
                        .name
                        .as_deref()
                        .map(codex::normalize_name)
                        .is_some_and(|name| {
                            preferred_agent_path_owner(
                                threads,
                                thread,
                                &name,
                                subagent.started_at_ms,
                            )
                        })
            })
    });
    let Some(child_id) = matches.next().map(|record| record.id.clone()) else {
        return;
    };
    if matches.next().is_some() {
        return;
    }
    if let Some(subagent) = records
        .get_mut(&child_id)
        .and_then(|record| record.subagent.as_mut())
    {
        subagent.parent_id = thread_record_id.to_string();
    }
}

fn preferred_agent_path_owner(
    threads: &[ThreadRollout],
    thread: &ThreadRollout,
    process_name: &str,
    process_started_at_ms: u64,
) -> bool {
    let mut candidates = threads
        .iter()
        .filter(|candidate| {
            !candidate.process_backed
                && candidate.parent_thread_id == thread.parent_thread_id
                && same_cwd(&candidate.cwd, &thread.cwd)
                && candidate.started_at_ms <= process_started_at_ms
                && candidate
                    .finished_at_ms
                    .is_none_or(|finished_at_ms| process_started_at_ms <= finished_at_ms)
        })
        .filter_map(|candidate| {
            agent_path_match_score(candidate, process_name)
                .map(|score| (candidate, (candidate.started_at_ms, score)))
        });
    let Some((best, score)) = candidates.next() else {
        return false;
    };
    let mut best_thread_id = best.thread_id.as_str();
    let mut best_score = score;
    let mut tied = false;
    for (candidate, score) in candidates {
        if score > best_score {
            best_thread_id = candidate.thread_id.as_str();
            best_score = score;
            tied = false;
        } else if score == best_score {
            tied = true;
        }
    }
    !tied && best_thread_id == thread.thread_id
}

fn agent_path_match_score(thread: &ThreadRollout, process_name: &str) -> Option<u8> {
    let path_name = thread
        .agent_path
        .as_deref()?
        .rsplit('/')
        .find(|component| !component.is_empty())
        .map(codex::normalize_name)?;
    let expected_name = path_name.strip_prefix("codex-").unwrap_or(&path_name);
    if expected_name == process_name {
        Some(1)
    } else {
        expected_name
            .strip_suffix(process_name)
            .filter(|prefix| prefix.ends_with('-'))
            .map(|_| 0)
    }
}

fn unique_root_parent(
    records: &HashMap<String, AgentRecord>,
    root: &RootRollout,
    record_starts: &HashMap<String, (String, u64)>,
) -> Option<String> {
    const ROOT_START_TOLERANCE_MS: u64 = 10_000;

    let mut matches = records
        .values()
        .filter(|record| {
            record.agent.eq_ignore_ascii_case("codex")
                && record.subagent.is_none()
                && matches!(
                    record.state,
                    AgentState::Working
                        | AgentState::Blocked
                        | AgentState::Idle
                        | AgentState::Unknown
                )
                && same_cwd(&record.cwd, &root.cwd)
                && record_starts.get(&record.id).is_some_and(|(_, started)| {
                    started.abs_diff(root.started_at_ms) <= ROOT_START_TOLERANCE_MS
                })
        })
        .map(|record| record.id.clone());
    let parent = matches.next()?;
    matches.next().is_none().then_some(parent)
}

fn suppress_finished_process_threads(
    records: &mut HashMap<String, AgentRecord>,
    record_pids: &mut HashMap<String, HashSet<u32>>,
    finished_process_threads: &mut HashMap<String, String>,
    threads: &[ThreadRollout],
) {
    let active_thread_ids = threads
        .iter()
        .filter(|thread| thread.finished_at_ms.is_none())
        .map(|thread| thread.thread_id.as_str())
        .collect::<HashSet<_>>();
    finished_process_threads.retain(|record_id, thread_id| {
        records.contains_key(record_id) && !active_thread_ids.contains(thread_id.as_str())
    });
    for record_id in finished_process_threads.keys() {
        records.remove(record_id);
        record_pids.remove(record_id);
    }
}

fn remember_finished_process_threads(
    records: &HashMap<String, AgentRecord>,
    host: &str,
    finished_process_threads: &mut HashMap<String, String>,
) {
    let synthetic_prefix = format!("{host}/codex-thread/");
    for record in records.values() {
        let Some(subagent) = record
            .subagent
            .as_ref()
            .filter(|subagent| subagent.finished_at_ms.is_some())
        else {
            continue;
        };
        if !record.id.starts_with(&synthetic_prefix)
            && let Some(thread_id) = &subagent.thread_id
        {
            finished_process_threads.insert(record.id.clone(), thread_id.clone());
        }
    }
}

fn insert_synthetic_thread(
    records: &mut HashMap<String, AgentRecord>,
    thread: &ThreadRollout,
    parent_id: &str,
    synthetic_id: &str,
    server: &str,
) -> bool {
    let Some(parent) = records.get(parent_id).cloned() else {
        return false;
    };
    let finished = thread.finished_at_ms;
    let state = if finished.is_some() {
        AgentState::Idle
    } else {
        AgentState::Working
    };
    let attention = if finished.is_some() {
        Attention::Done
    } else {
        Attention::Working
    };
    let name = thread.name.clone();
    records.insert(
        synthetic_id.to_string(),
        AgentRecord {
            id: synthetic_id.to_string(),
            host: parent.host,
            server: server.to_string(),
            pane_id: parent.pane_id,
            pane_pid: parent.pane_pid,
            session_id: parent.session_id,
            session_name: parent.session_name,
            window_id: parent.window_id,
            window_index: parent.window_index,
            window_name: parent.window_name,
            pane_index: parent.pane_index,
            agent: "Codex".into(),
            state,
            attention,
            source: EvidenceSource::Process,
            title: name.clone().unwrap_or_else(|| "subagent".into()),
            label: None,
            cwd: thread.cwd.clone(),
            visible: false,
            seen: false,
            changed_at_ms: finished.unwrap_or(thread.started_at_ms),
            origin: parent.origin,
            terminal: parent.terminal,
            remote_alias: None,
            ssh_connection: parent.ssh_connection,
            focus_target: None,
            goal: None,
            subagent: Some(SubagentInfo {
                parent_id: parent_id.to_string(),
                started_at_ms: thread.started_at_ms,
                finished_at_ms: finished,
                name,
                thread_id: Some(thread.thread_id.clone()),
            }),
            detection: None,
        },
    );
    true
}

fn unambiguous_thread_parents(
    record_thread_ids: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut candidates = HashMap::<String, Vec<String>>::new();
    for (record_id, thread_id) in record_thread_ids {
        candidates
            .entry(thread_id.clone())
            .or_default()
            .push(record_id.clone());
    }
    candidates
        .into_iter()
        .filter_map(|(thread_id, records)| {
            (records.len() == 1).then(|| (thread_id, records[0].clone()))
        })
        .collect()
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

fn unique_cwd_parent(
    records: &HashMap<String, AgentRecord>,
    thread: &ThreadRollout,
    record_starts: &HashMap<String, (String, u64)>,
) -> Option<String> {
    if thread.cwd.is_empty() {
        return None;
    }
    let mut matches = records
        .values()
        .filter(|record| {
            record.agent.eq_ignore_ascii_case("codex")
                && record.subagent.is_none()
                && matches!(
                    record.state,
                    AgentState::Working | AgentState::Blocked | AgentState::Unknown
                )
                && record_starts.get(&record.id).is_some_and(|(_, started)| {
                    *started <= thread.started_at_ms.saturating_add(2_000)
                })
                && same_cwd(&record.cwd, &thread.cwd)
        })
        .map(|record| record.id.clone());
    let parent = matches.next()?;
    matches.next().is_none().then_some(parent)
}

fn same_cwd(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn attach_thread_to_process_child(
    records: &mut HashMap<String, AgentRecord>,
    threads: &[ThreadRollout],
    thread: &ThreadRollout,
    parent_id: &str,
    previous: &HashMap<String, AgentRecord>,
) -> Option<String> {
    let base_candidate = |record: &AgentRecord| {
        record.agent.eq_ignore_ascii_case("codex")
            && record
                .subagent
                .as_ref()
                .is_some_and(|subagent| subagent.parent_id == parent_id)
    };
    let mut exact = records
        .iter()
        .filter(|(_, record)| {
            record.agent.eq_ignore_ascii_case("codex")
                && is_subagent_descendant_of(records, record, parent_id)
                && record
                    .subagent
                    .as_ref()
                    .and_then(|subagent| subagent.thread_id.as_deref())
                    == Some(thread.thread_id.as_str())
        })
        .map(|(id, _)| id.clone());
    if let Some(candidate) = exact.next() {
        if exact.next().is_none() {
            return update_process_child(records, thread, candidate);
        }
        return None;
    }
    let mut candidates = records
        .iter()
        .filter(|(record_id, record)| {
            if !base_candidate(record) || !same_cwd(&record.cwd, &thread.cwd) {
                return false;
            }
            heuristic_process_match(record_id, record, thread, parent_id, previous)
                && threads
                    .iter()
                    .filter(|candidate_thread| {
                        candidate_thread.parent_thread_id == thread.parent_thread_id
                            && heuristic_process_match(
                                record_id,
                                record,
                                candidate_thread,
                                parent_id,
                                previous,
                            )
                    })
                    .take(2)
                    .count()
                    == 1
        })
        .map(|(id, _)| id.clone());
    if let Some(candidate) = candidates.next() {
        if candidates.next().is_some() {
            return None;
        }
        return update_process_child(records, thread, candidate);
    }

    let mut nested_candidates = records
        .iter()
        .filter(|(record_id, record)| {
            heuristic_nested_process_match(records, record_id, record, thread, parent_id)
                && threads
                    .iter()
                    .filter(|candidate_thread| {
                        candidate_thread.parent_thread_id == thread.parent_thread_id
                            && heuristic_nested_process_match(
                                records,
                                record_id,
                                record,
                                candidate_thread,
                                parent_id,
                            )
                    })
                    .take(2)
                    .count()
                    == 1
        })
        .map(|(id, _)| id.clone());
    let candidate = nested_candidates.next()?;
    if nested_candidates.next().is_some() {
        return None;
    }
    update_process_child(records, thread, candidate)
}

fn heuristic_process_match(
    _record_id: &str,
    record: &AgentRecord,
    thread: &ThreadRollout,
    parent_id: &str,
    _previous: &HashMap<String, AgentRecord>,
) -> bool {
    if !thread.process_backed {
        return false;
    }
    let Some(subagent) = &record.subagent else {
        return false;
    };
    if !record.agent.eq_ignore_ascii_case("codex")
        || subagent.parent_id != parent_id
        || !same_cwd(&record.cwd, &thread.cwd)
        || subagent.thread_id.is_some()
    {
        return false;
    }
    let expected_name = thread.name.as_deref().map(codex::normalize_name);
    let candidate_name = subagent.name.as_deref().map(codex::normalize_name);
    (subagent.started_at_ms.abs_diff(thread.started_at_ms) <= 120_000)
        && (expected_name.is_none() || candidate_name.is_none() || expected_name == candidate_name)
}

fn heuristic_nested_process_match(
    records: &HashMap<String, AgentRecord>,
    _record_id: &str,
    record: &AgentRecord,
    thread: &ThreadRollout,
    parent_id: &str,
) -> bool {
    if !thread.process_backed || !is_subagent_descendant_of(records, record, parent_id) {
        return false;
    }
    let Some(subagent) = &record.subagent else {
        return false;
    };
    if subagent.parent_id == parent_id
        || !record.agent.eq_ignore_ascii_case("codex")
        || !same_cwd(&record.cwd, &thread.cwd)
        || subagent.thread_id.is_some()
    {
        return false;
    }
    let expected_name = thread.name.as_deref().map(codex::normalize_name);
    let candidate_name = subagent.name.as_deref().map(codex::normalize_name);
    (subagent.started_at_ms.abs_diff(thread.started_at_ms) <= 120_000)
        && (expected_name.is_none() || candidate_name.is_none() || expected_name == candidate_name)
}

fn is_subagent_descendant_of(
    records: &HashMap<String, AgentRecord>,
    record: &AgentRecord,
    ancestor_id: &str,
) -> bool {
    let Some(mut parent_id) = record
        .subagent
        .as_ref()
        .map(|subagent| subagent.parent_id.as_str())
    else {
        return false;
    };
    let mut visited = HashSet::new();
    loop {
        if parent_id == ancestor_id {
            return true;
        }
        if !visited.insert(parent_id.to_string()) {
            return false;
        }
        let Some(parent) = records.get(parent_id) else {
            return false;
        };
        let Some(subagent) = &parent.subagent else {
            return false;
        };
        parent_id = &subagent.parent_id;
    }
}

fn update_process_child(
    records: &mut HashMap<String, AgentRecord>,
    thread: &ThreadRollout,
    candidate: String,
) -> Option<String> {
    let record = records.get_mut(&candidate)?;
    let subagent = record.subagent.as_mut()?;
    subagent.thread_id = Some(thread.thread_id.clone());
    subagent.started_at_ms = thread.started_at_ms;
    if subagent.name.is_none() {
        subagent.name = thread.name.clone();
    }
    if let Some(finished_at_ms) = thread.finished_at_ms {
        subagent.finished_at_ms = Some(finished_at_ms);
        record.state = AgentState::Idle;
        record.attention = Attention::Done;
        record.seen = false;
        record.changed_at_ms = finished_at_ms;
    }
    Some(candidate)
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
        let mut detection = detect::detect("/opt/homebrew/bin/codex", "", "").unwrap();
        mark_process_only(&mut detection);
        assert_eq!(detection.state, AgentState::Unknown);
        assert_eq!(detection.source, EvidenceSource::Process);
        assert!(detection.details.is_none());
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

    #[test]
    fn resumed_codex_thread_links_by_exact_id_despite_wrapper_cwd() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/wrapper-home".into();
        parent.origin = AgentOrigin::Terminal;
        let parent_id = parent.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent)]);
        let thread_ids = HashMap::from([(
            parent_id.clone(),
            "01800000-0000-7000-8000-000000000001".into(),
        )]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        let child = &records["host/codex-thread/01800000-0000-7000-8000-000000000002"];
        assert_eq!(child.attention, Attention::Working);
        assert_eq!(
            child
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(parent_id.as_str())
        );
        assert_eq!(
            child
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.name.as_deref()),
            Some("Worker")
        );
    }

    #[test]
    fn agent_path_nests_process_child_under_in_process_thread() {
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
        let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
        let mut thread = codex_thread(None);
        thread.agent_path = Some("/root/codex_review".into());
        thread.process_backed = false;

        link_codex_thread_subagents(
            &mut records,
            &[thread],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        let synthetic_id = "host/codex-thread/01800000-0000-7000-8000-000000000002";
        let worker = &records[synthetic_id];
        assert_eq!(
            worker
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some("host/run/main")
        );
        assert_eq!(
            records[&review_id]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(synthetic_id)
        );
    }

    #[test]
    fn task_specific_agent_path_nests_role_process_child() {
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
        let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
        let mut thread = codex_thread(None);
        thread.agent_path = Some("/root/final_memory_recovery_review".into());
        thread.process_backed = false;

        link_codex_thread_subagents(
            &mut records,
            &[thread],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        let synthetic_id = "host/codex-thread/01800000-0000-7000-8000-000000000002";
        assert_eq!(
            records[&review_id]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(synthetic_id)
        );
    }

    #[test]
    fn task_specific_agent_path_does_not_reparent_older_process_child() {
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
            started_at_ms: 4_500,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let review_id = review.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
        let thread_ids = HashMap::from([(
            parent_id.clone(),
            "01800000-0000-7000-8000-000000000001".into(),
        )]);
        let mut thread = codex_thread(None);
        thread.agent_path = Some("/root/final_memory_recovery_review".into());
        thread.process_backed = false;

        link_codex_thread_subagents(
            &mut records,
            &[thread],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records[&review_id]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(parent_id.as_str())
        );
    }

    #[test]
    fn completed_role_thread_does_not_claim_later_process_child() {
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
            started_at_ms: 6_600,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let review_id = review.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
        let thread_ids = HashMap::from([(parent_id, "root-thread".into())]);
        let mut completed = codex_thread(Some(6_000));
        completed.thread_id = "completed-review".into();
        completed.parent_thread_id = "root-thread".into();
        completed.agent_path = Some("/root/first_task_review".into());
        completed.process_backed = false;
        let mut active = codex_thread(None);
        active.thread_id = "active-review".into();
        active.parent_thread_id = "root-thread".into();
        active.started_at_ms = 6_500;
        active.agent_path = Some("/root/second_task_review".into());
        active.process_backed = false;

        link_codex_thread_subagents(
            &mut records,
            &[completed, active],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records[&review_id]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some("host/codex-thread/active-review")
        );
    }

    #[test]
    fn latest_overlapping_role_thread_claims_process_child() {
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
            started_at_ms: 6_600,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let review_id = review.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
        let thread_ids = HashMap::from([(parent_id, "root-thread".into())]);
        let mut first = codex_thread(None);
        first.thread_id = "first-review".into();
        first.parent_thread_id = "root-thread".into();
        first.agent_path = Some("/root/first_task_review".into());
        first.process_backed = false;
        let mut second = codex_thread(None);
        second.thread_id = "second-review".into();
        second.parent_thread_id = "root-thread".into();
        second.started_at_ms = 6_500;
        second.agent_path = Some("/root/second_task_review".into());
        second.process_backed = false;

        link_codex_thread_subagents(
            &mut records,
            &[first, second],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records[&review_id]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some("host/codex-thread/second-review")
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
        insert_synthetic_thread(&mut previous, &thread, &parent_id, &synthetic_id, "default");
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
    fn codex_thread_fallback_refuses_ambiguous_same_cwd_parents() {
        let mut first = old(AgentState::Working, true);
        first.id = "host/default/%1".into();
        first.cwd = "/work".into();
        let mut second = old(AgentState::Working, true);
        second.id = "host/default/%2".into();
        second.cwd = "/work".into();
        let mut records = HashMap::from([(first.id.clone(), first), (second.id.clone(), second)]);
        let record_starts = HashMap::from([
            ("host/default/%1".into(), ("Codex:1".into(), 1_000)),
            ("host/default/%2".into(), ("Codex:2".into(), 1_000)),
        ]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &HashMap::new(),
            &record_starts,
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(records.len(), 2);
        assert!(records.values().all(|record| record.subagent.is_none()));
    }

    #[test]
    fn codex_thread_fallback_rejects_a_different_known_root_session() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/default/%1".into();
        parent.cwd = "/work".into();
        let mut records = HashMap::from([(parent.id.clone(), parent)]);
        let record_starts = HashMap::from([("host/default/%1".into(), ("Codex:1".into(), 50_000))]);
        let root_rollouts = HashMap::from([(
            "01800000-0000-7000-8000-000000000001".into(),
            RootRollout {
                cwd: "/work".into(),
                started_at_ms: 1_000,
            },
        )]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &root_rollouts,
            &HashMap::new(),
            &record_starts,
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(records.len(), 1);
        assert!(records.values().all(|record| record.subagent.is_none()));
    }

    #[test]
    fn codex_thread_fallback_matches_a_fresh_known_root_session_by_start_time() {
        let mut parent = old(AgentState::Idle, true);
        parent.id = "host/default/%1".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent)]);
        let record_starts = HashMap::from([(parent_id.clone(), ("Codex:1".into(), 1_500))]);
        let root_rollouts = HashMap::from([(
            "01800000-0000-7000-8000-000000000001".into(),
            RootRollout {
                cwd: "/work".into(),
                started_at_ms: 1_000,
            },
        )]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &root_rollouts,
            &HashMap::new(),
            &record_starts,
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records["host/codex-thread/01800000-0000-7000-8000-000000000002"]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(parent_id.as_str())
        );
    }

    #[test]
    fn codex_thread_fallback_matches_picker_resumed_cached_root_by_cwd() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/default/%1".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent)]);
        let record_starts = HashMap::from([(parent_id.clone(), ("Codex:1".into(), 4_000))]);
        let root_rollouts = HashMap::from([(
            "01800000-0000-7000-8000-000000000001".into(),
            RootRollout {
                cwd: "/work".into(),
                started_at_ms: 1_000,
            },
        )]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &root_rollouts,
            &HashMap::new(),
            &record_starts,
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records["host/codex-thread/01800000-0000-7000-8000-000000000002"]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(parent_id.as_str())
        );
    }

    #[test]
    fn codex_thread_fallback_refuses_parent_started_after_thread() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/default/%1".into();
        parent.cwd = "/work".into();
        parent.changed_at_ms = 6_000;
        let mut records = HashMap::from([(parent.id.clone(), parent)]);
        let record_starts = HashMap::from([("host/default/%1".into(), ("Codex:1".into(), 8_000))]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &HashMap::new(),
            &record_starts,
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(records.len(), 1);
        assert!(records.values().all(|record| record.subagent.is_none()));
    }

    #[test]
    fn codex_thread_fallback_uses_stable_start_across_parent_state_changes() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/default/%1".into();
        parent.cwd = "/work".into();
        parent.changed_at_ms = 6_000;
        let parent_id = parent.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent)]);
        let record_starts = HashMap::from([(parent_id.clone(), ("Codex:1".into(), 1_000))]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &HashMap::new(),
            &record_starts,
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records["host/codex-thread/01800000-0000-7000-8000-000000000002"]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(parent_id.as_str())
        );
    }

    #[test]
    fn codex_thread_fallback_accepts_unknown_process_only_parent() {
        let mut parent = old(AgentState::Unknown, true);
        parent.id = "host/terminal/ttys001/10".into();
        parent.cwd = "/work".into();
        parent.origin = AgentOrigin::Terminal;
        let parent_id = parent.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent)]);
        let record_starts = HashMap::from([(parent_id.clone(), ("Codex:10".into(), 1_000))]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &HashMap::new(),
            &record_starts,
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records["host/codex-thread/01800000-0000-7000-8000-000000000002"]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some(parent_id.as_str())
        );
    }

    #[test]
    fn rollout_identity_enriches_process_child_without_duplication() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.cwd = "/work".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 5_010,
            finished_at_ms: None,
            name: None,
            thread_id: None,
        });
        let parent_id = parent.id.clone();
        let child_id = child.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(Some(9_000))],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(records.len(), 2);
        let child = &records[&child_id];
        assert_eq!(child.attention, Attention::Done);
        assert_eq!(
            child
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_deref()),
            Some("01800000-0000-7000-8000-000000000002")
        );
        assert_eq!(
            child
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.name.as_deref()),
            Some("Worker")
        );
    }

    #[test]
    fn process_backed_rollout_enriches_child_nested_under_in_process_thread() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut delegated = old(AgentState::Idle, true);
        delegated.id = "host/codex-thread/delegated".into();
        delegated.cwd = "/work".into();
        delegated.subagent = Some(SubagentInfo {
            parent_id: parent_id.clone(),
            started_at_ms: 4_000,
            finished_at_ms: Some(6_000),
            name: Some("Banach".into()),
            thread_id: Some("delegated".into()),
        });
        let delegated_id = delegated.id.clone();
        let mut review = old(AgentState::Unknown, true);
        review.id = "host/terminal/ttys002/70".into();
        review.cwd = "/work".into();
        review.origin = AgentOrigin::Terminal;
        review.subagent = Some(SubagentInfo {
            parent_id: delegated_id,
            started_at_ms: 5_010,
            finished_at_ms: None,
            name: Some("Worker".into()),
            thread_id: None,
        });
        let review_id = review.id.clone();
        let mut records = HashMap::from([
            (parent_id.clone(), parent),
            (delegated.id.clone(), delegated),
            (review_id.clone(), review),
        ]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(records.len(), 3);
        assert_eq!(
            records[&review_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_deref()),
            Some("01800000-0000-7000-8000-000000000002")
        );
        assert!(!records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
    }

    #[test]
    fn in_process_thread_does_not_replace_a_separate_process_child() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.cwd = "/work".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: parent_id.clone(),
            started_at_ms: 5_010,
            finished_at_ms: None,
            name: Some("Worker".into()),
            thread_id: None,
        });
        let child_id = child.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
        let mut thread = codex_thread(None);
        thread.process_backed = false;

        link_codex_thread_subagents(
            &mut records,
            &[thread],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(records.len(), 3);
        assert!(
            records[&child_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_ref())
                .is_none()
        );
        assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
    }

    #[test]
    fn rollout_identity_rejects_new_process_child_with_mismatched_start_time() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.cwd = "/work".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 500_000,
            finished_at_ms: None,
            name: Some("Worker".into()),
            thread_id: None,
        });
        let parent_id = parent.id.clone();
        let child_id = child.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(records.len(), 3);
        assert!(
            records[&child_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_ref())
                .is_none()
        );
        assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
    }

    #[test]
    fn rollout_identity_does_not_overwrite_process_child_bound_to_another_thread() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.cwd = "/work".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 5_000,
            finished_at_ms: None,
            name: None,
            thread_id: Some("already-bound".into()),
        });
        let parent_id = parent.id.clone();
        let child_id = child.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records[&child_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_deref()),
            Some("already-bound")
        );
        assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
    }

    #[test]
    fn rollout_identity_does_not_enrich_a_non_codex_process_child() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.agent = "Claude".into();
        child.cwd = "/work".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 5_000,
            finished_at_ms: None,
            name: None,
            thread_id: None,
        });
        let parent_id = parent.id.clone();
        let child_id = child.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert!(
            records[&child_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_ref())
                .is_none()
        );
        assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
    }

    #[test]
    fn exact_thread_binding_outranks_an_unbound_process_child() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut exact = old(AgentState::Unknown, true);
        exact.id = "host/terminal/ttys002/70".into();
        exact.cwd = "/work".into();
        exact.origin = AgentOrigin::Terminal;
        exact.subagent = Some(SubagentInfo {
            parent_id: parent_id.clone(),
            started_at_ms: 5_000,
            finished_at_ms: None,
            name: None,
            thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
        });
        let exact_id = exact.id.clone();
        let mut unbound = exact.clone();
        unbound.id = "host/terminal/ttys003/80".into();
        unbound.subagent.as_mut().unwrap().thread_id = None;
        let unbound_id = unbound.id.clone();
        let mut records = HashMap::from([
            (parent_id.clone(), parent),
            (exact_id.clone(), exact),
            (unbound_id.clone(), unbound),
        ]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(Some(9_000))],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records[&exact_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.finished_at_ms),
            Some(9_000)
        );
        assert!(
            records[&unbound_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_ref())
                .is_none()
        );
        assert!(!records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
    }

    #[test]
    fn exact_thread_binding_does_not_require_matching_cwd() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.cwd = "/work-before-runner-refresh".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: parent_id.clone(),
            started_at_ms: 5_000,
            finished_at_ms: None,
            name: None,
            thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
        });
        let child_id = child.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(Some(9_000))],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records[&child_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.finished_at_ms),
            Some(9_000)
        );
        assert!(!records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
    }

    #[test]
    fn competing_rollouts_do_not_claim_one_unbound_process_child() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut child = old(AgentState::Unknown, true);
        child.id = "host/terminal/ttys002/70".into();
        child.cwd = "/work".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: parent_id.clone(),
            started_at_ms: 5_000,
            finished_at_ms: None,
            name: None,
            thread_id: None,
        });
        let child_id = child.id.clone();
        let mut first = codex_thread(None);
        first.thread_id = "first-thread".into();
        first.name = None;
        let mut second = codex_thread(None);
        second.thread_id = "second-thread".into();
        second.name = None;
        let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

        link_codex_thread_subagents(
            &mut records,
            &[second, first],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert!(
            records[&child_id]
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_ref())
                .is_none()
        );
        assert!(records.contains_key("host/codex-thread/first-thread"));
        assert!(records.contains_key("host/codex-thread/second-thread"));
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

        link_codex_thread_subagents(
            &mut records,
            &[codex_thread(None)],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &previous,
            ("host", "default"),
        );
        retain_finished_subagents(&mut records, &previous, 10_000);

        assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
        assert!(!records.contains_key("host/terminal/ttys002/70"));
    }

    #[test]
    fn completed_process_thread_stays_suppressed_until_reactivation() {
        let mut child = old(AgentState::Idle, false);
        child.id = "host/terminal/ttys002/70".into();
        child.origin = AgentOrigin::Terminal;
        child.subagent = Some(SubagentInfo {
            parent_id: "host/run/main".into(),
            started_at_ms: 5_000,
            finished_at_ms: Some(10_000),
            name: Some("Worker".into()),
            thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
        });
        let previous = HashMap::from([(child.id.clone(), child.clone())]);
        let mut tombstones = HashMap::new();
        remember_finished_process_threads(&previous, "host", &mut tombstones);

        let mut records = HashMap::from([(child.id.clone(), child.clone())]);
        let mut record_pids = HashMap::from([(child.id.clone(), HashSet::from([70]))]);
        suppress_finished_process_threads(&mut records, &mut record_pids, &mut tombstones, &[]);
        retain_finished_subagents(&mut records, &previous, 40_000);
        assert!(records.is_empty());
        assert!(record_pids.is_empty());
        assert_eq!(
            tombstones.get(&child.id).map(String::as_str),
            Some("01800000-0000-7000-8000-000000000002")
        );

        let mut reactivated_records = HashMap::from([(child.id.clone(), child)]);
        let mut reactivated_pids =
            HashMap::from([("host/terminal/ttys002/70".into(), HashSet::from([70]))]);
        suppress_finished_process_threads(
            &mut reactivated_records,
            &mut reactivated_pids,
            &mut tombstones,
            &[codex_thread(None)],
        );
        assert!(reactivated_records.contains_key("host/terminal/ttys002/70"));
        assert!(tombstones.is_empty());
    }

    #[test]
    fn nested_codex_thread_links_to_its_synthetic_thread_parent() {
        let mut parent = old(AgentState::Working, true);
        parent.id = "host/run/main".into();
        parent.cwd = "/work".into();
        let parent_id = parent.id.clone();
        let mut records = HashMap::from([(parent_id.clone(), parent)]);
        let thread_ids =
            HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
        let mut first = codex_thread(None);
        first.thread_id = "first-thread".into();
        first.name = Some("First".into());
        let mut nested = codex_thread(None);
        nested.thread_id = "nested-thread".into();
        nested.parent_thread_id = first.thread_id.clone();
        nested.name = Some("Nested".into());
        nested.depth = Some(2);

        link_codex_thread_subagents(
            &mut records,
            &[nested, first],
            &HashMap::new(),
            &thread_ids,
            &HashMap::new(),
            &HashMap::new(),
            ("host", "default"),
        );

        assert_eq!(
            records["host/codex-thread/nested-thread"]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some("host/codex-thread/first-thread")
        );
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
