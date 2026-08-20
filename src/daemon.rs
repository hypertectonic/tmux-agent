use crate::config::{Config, RemoteConfig, RuntimePaths};
use crate::model::{
    APPLICATION_VERSION, AcknowledgedState, AgentRecord, AgentState, Attention,
    GoalAcknowledgement, GoalState, IpcRequest, IpcResponse, PROTOCOL_VERSION, PeerStatus,
    Snapshot, SshTransport,
};
use crate::scanner::{Scanner, now_ms};
use crate::{
    store,
    tmux::{Tmux, is_server_missing},
};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{RwLock, watch};

const MISSING_SERVER_EXIT_THRESHOLD: u8 = 3;

enum ScanLoopExit {
    ServerMissing,
}

enum ScanErrorAction {
    Retry,
    Exit,
    Report,
}

#[derive(Default)]
struct MissingServerPolicy {
    consecutive: u8,
}

impl MissingServerPolicy {
    fn success(&mut self) {
        self.consecutive = 0;
    }

    fn error(&mut self, error: &anyhow::Error) -> ScanErrorAction {
        if !is_server_missing(error) {
            self.consecutive = 0;
            return ScanErrorAction::Report;
        }
        self.consecutive += 1;
        if self.consecutive >= MISSING_SERVER_EXIT_THRESHOLD {
            ScanErrorAction::Exit
        } else {
            ScanErrorAction::Retry
        }
    }
}

enum RemoteStreamExit {
    Ended,
    Shutdown,
}

struct Aggregate {
    local: Snapshot,
    remotes: HashMap<String, Snapshot>,
    peers: HashMap<String, PeerStatus>,
    acknowledgements: Acknowledgements,
    last_used_at_ms: HashMap<String, u64>,
    revision: u64,
}

#[derive(Debug, Default)]
struct Acknowledgements {
    completions: HashSet<String>,
    goal_achievements: HashMap<String, u64>,
}

impl Acknowledgements {
    fn from_state(state: AcknowledgedState) -> Self {
        Self {
            completions: state.ids.into_iter().collect(),
            goal_achievements: state
                .goal_achievements
                .into_iter()
                .map(|goal| (goal.id, goal.achievement_observed_at_ms))
                .collect(),
        }
    }

    fn state(&self) -> AcknowledgedState {
        let mut ids = self.completions.iter().cloned().collect::<Vec<_>>();
        ids.sort();
        let mut goal_achievements = self
            .goal_achievements
            .iter()
            .map(|(id, achievement_observed_at_ms)| GoalAcknowledgement {
                id: id.clone(),
                achievement_observed_at_ms: *achievement_observed_at_ms,
            })
            .collect::<Vec<_>>();
        goal_achievements.sort_by(|left, right| left.id.cmp(&right.id));
        AcknowledgedState {
            protocol: PROTOCOL_VERSION,
            ids,
            goal_achievements,
        }
    }
}

struct Shared {
    inner: RwLock<Aggregate>,
    changed: watch::Sender<u64>,
    local_changed: watch::Sender<u64>,
    acknowledgements_path: PathBuf,
}

impl Shared {
    fn new(
        mut local: Snapshot,
        remotes: &[RemoteConfig],
        mut acknowledgements: Acknowledgements,
        acknowledgements_path: PathBuf,
    ) -> Result<Arc<Self>> {
        if apply_acknowledgements(&mut local.agents, &mut acknowledgements) {
            store::save_acknowledged(&acknowledgements_path, &acknowledgements.state())?;
        }
        let peers = remotes
            .iter()
            .map(|remote| {
                (
                    remote.name.clone(),
                    PeerStatus {
                        name: remote.name.clone(),
                        connected: false,
                        last_error: None,
                        application_version: None,
                        protocol: 0,
                        capabilities: Vec::new(),
                    },
                )
            })
            .collect();
        let (changed, _) = watch::channel(1);
        let (local_changed, _) = watch::channel(local.revision);
        Ok(Arc::new(Self {
            inner: RwLock::new(Aggregate {
                local,
                remotes: HashMap::new(),
                peers,
                acknowledgements,
                last_used_at_ms: HashMap::new(),
                revision: 1,
            }),
            changed,
            local_changed,
            acknowledgements_path,
        }))
    }

    async fn publish_local(&self, mut snapshot: Snapshot) -> bool {
        let mut inner = self.inner.write().await;
        let acknowledgement_changed =
            apply_acknowledgements(&mut snapshot.agents, &mut inner.acknowledgements);
        if inner.local.agents == snapshot.agents
            && inner.local.host == snapshot.host
            && inner.local.server == snapshot.server
            && inner.local.ssh_transports == snapshot.ssh_transports
        {
            if acknowledgement_changed {
                self.persist_acknowledgements(&inner.acknowledgements);
            }
            return false;
        }
        let local_revision = snapshot.revision;
        inner.local = snapshot;
        prune_last_used(&mut inner);
        inner.revision += 1;
        let revision = inner.revision;
        if acknowledgement_changed {
            self.persist_acknowledgements(&inner.acknowledgements);
        }
        drop(inner);
        self.changed.send_replace(revision);
        self.local_changed.send_replace(local_revision);
        true
    }

    async fn publish_remote(&self, alias: &str, mut snapshot: Snapshot) {
        snapshot.peers.clear();
        namespace_remote(alias, &mut snapshot);
        let application_version = snapshot.application_version.clone();
        let protocol = snapshot.protocol;
        let capabilities = snapshot.capabilities.clone();
        let mut inner = self.inner.write().await;
        let acknowledgement_changed =
            apply_acknowledgements(&mut snapshot.agents, &mut inner.acknowledgements);
        inner.remotes.insert(alias.to_string(), snapshot);
        prune_last_used(&mut inner);
        if let Some(peer) = inner.peers.get_mut(alias) {
            peer.connected = true;
            peer.last_error = None;
            peer.application_version = application_version;
            peer.protocol = protocol;
            peer.capabilities = capabilities;
        }
        inner.revision += 1;
        let revision = inner.revision;
        if acknowledgement_changed {
            self.persist_acknowledgements(&inner.acknowledgements);
        }
        drop(inner);
        self.changed.send_replace(revision);
    }

    async fn peer_error(&self, alias: &str, message: String) {
        let mut inner = self.inner.write().await;
        if let Some(peer) = inner.peers.get_mut(alias) {
            peer.connected = false;
            peer.last_error = Some(message);
        }
        inner.remotes.remove(alias);
        prune_last_used(&mut inner);
        inner.revision += 1;
        let revision = inner.revision;
        drop(inner);
        self.changed.send_replace(revision);
    }

    async fn acknowledge(&self, target: &str) -> Result<bool> {
        let mut inner = self.inner.write().await;
        let local = acknowledge_records(&mut inner.local.agents, target);
        let remote = inner
            .remotes
            .values_mut()
            .find_map(|snapshot| {
                let result = acknowledge_records(&mut snapshot.agents, target);
                result.found.then_some(result)
            })
            .unwrap_or_default();
        if !local.found && !remote.found {
            return Ok(false);
        }
        if local.persist_completion || remote.persist_completion {
            inner
                .acknowledgements
                .completions
                .insert(target.to_string());
        }
        if let Some(achievement_observed_at_ms) = local.goal_achievement.or(remote.goal_achievement)
        {
            inner
                .acknowledgements
                .goal_achievements
                .insert(target.to_string(), achievement_observed_at_ms);
        }
        if local.persist_completion
            || remote.persist_completion
            || local.goal_achievement.is_some()
            || remote.goal_achievement.is_some()
        {
            store::save_acknowledged(&self.acknowledgements_path, &inner.acknowledgements.state())?;
        }
        if local.found {
            inner.local.revision += 1;
        }
        inner.revision += 1;
        let revision = inner.revision;
        let local_revision = inner.local.revision;
        drop(inner);
        self.changed.send_replace(revision);
        if local.found {
            self.local_changed.send_replace(local_revision);
        }
        Ok(true)
    }

    async fn mark_used(&self, target: &str) -> bool {
        let mut inner = self.inner.write().await;
        let is_top_level = inner
            .local
            .agents
            .iter()
            .chain(
                inner
                    .remotes
                    .values()
                    .flat_map(|snapshot| snapshot.agents.iter()),
            )
            .any(|agent| agent.id == target && agent.subagent.is_none());
        if !is_top_level {
            return false;
        }
        inner.last_used_at_ms.insert(target.to_string(), now_ms());
        inner.revision += 1;
        let revision = inner.revision;
        drop(inner);
        self.changed.send_replace(revision);
        true
    }

    async fn snapshot(&self, local_only: bool) -> Snapshot {
        let inner = self.inner.read().await;
        if local_only {
            return inner.local.clone();
        }
        let mut snapshot = inner.local.clone();
        snapshot.revision = inner.revision;
        snapshot.generated_at_ms = now_ms();
        for remote in inner.remotes.values() {
            let mut agents = remote.agents.clone();
            reconcile_transports(&mut agents, &inner.local.ssh_transports);
            snapshot.agents.extend(agents);
        }
        snapshot.peers = inner.peers.values().cloned().collect();
        snapshot.sort_agents_by_last_used(&inner.last_used_at_ms);
        snapshot
    }

    fn persist_acknowledgements(&self, acknowledgements: &Acknowledgements) {
        if let Err(error) =
            store::save_acknowledged(&self.acknowledgements_path, &acknowledgements.state())
        {
            eprintln!("tmux-agent: persist acknowledgements: {error:#}");
        }
    }
}

fn prune_last_used(inner: &mut Aggregate) {
    let known_ids = inner
        .local
        .agents
        .iter()
        .chain(
            inner
                .remotes
                .values()
                .flat_map(|snapshot| snapshot.agents.iter()),
        )
        .map(|agent| agent.id.clone())
        .collect::<HashSet<_>>();
    inner
        .last_used_at_ms
        .retain(|agent_id, _| known_ids.contains(agent_id));
}

pub async fn run(config: Config, paths: RuntimePaths, tmux: Tmux) -> Result<()> {
    paths.ensure_dirs()?;
    if paths.socket.exists() {
        if UnixStream::connect(&paths.socket).await.is_ok() {
            bail!("daemon is already running at {}", paths.socket.display());
        }
        fs::remove_file(&paths.socket)
            .with_context(|| format!("remove stale socket {}", paths.socket.display()))?;
    }

    let discovered_server_key = tmux.server_key()?;
    let tmux_server_observed = discovered_server_key.is_some();
    let server_key = discovered_server_key.unwrap_or_else(|| tmux.runtime_key());
    let persisted = store::load(&paths.state).unwrap_or_default();
    let acknowledgements = store::load_acknowledged(&paths.acknowledgements)
        .ok()
        .filter(|state| state.protocol == PROTOCOL_VERSION)
        .map(Acknowledgements::from_state)
        .unwrap_or_default();
    let scanner = Scanner::new(
        &config,
        tmux.clone(),
        &server_key,
        paths.runners.clone(),
        persisted,
        tmux_server_observed,
    )?;
    let scan_interval = Duration::from_millis(config.scan_interval_ms());
    let (scanner, first) = match bootstrap_scan(scanner, scan_interval).await? {
        Some(ready) => ready,
        None => return Ok(()),
    };
    store::save(&paths.state, &scanner.persisted())?;
    let collectors = config.collectors();
    let shared = Shared::new(
        first,
        &collectors,
        acknowledgements,
        paths.acknowledgements.clone(),
    )?;
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("bind daemon socket {}", paths.socket.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure daemon socket {}", paths.socket.display()))?;
    let (collector_shutdown, collector_shutdown_rx) = watch::channel(false);
    let collector_tasks = collectors
        .into_iter()
        .map(|remote| {
            tokio::spawn(remote_loop(
                remote,
                shared.clone(),
                collector_shutdown_rx.clone(),
            ))
        })
        .collect::<Vec<_>>();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    let mut listener_task = tokio::spawn(accept_clients(
        listener,
        shared.clone(),
        shutdown_tx.clone(),
    ));
    let mut scanner_task = tokio::spawn(scan_loop(
        scanner,
        shared.clone(),
        paths.state.clone(),
        scan_interval,
    ));
    let mut listener_finished = false;
    let mut scanner_finished = false;
    let outcome = tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("wait for shutdown signal")
        }
        _ = shutdown_rx.recv() => Ok(()),
        result = &mut listener_task => {
            listener_finished = true;
            match result {
                Ok(Ok(())) => Err(anyhow::anyhow!("daemon listener stopped unexpectedly")),
                Ok(Err(error)) => Err(error.context("daemon listener stopped")),
                Err(error) => Err(anyhow::Error::new(error).context("join daemon listener task")),
            }
        }
        result = &mut scanner_task => {
            scanner_finished = true;
            match result {
                Ok(Ok(ScanLoopExit::ServerMissing)) => Ok(()),
                Ok(Err(error)) => Err(error.context("daemon scanner stopped")),
                Err(error) => Err(anyhow::Error::new(error).context("join daemon scanner task")),
            }
        }
    };
    collector_shutdown.send_replace(true);
    if !listener_finished {
        listener_task.abort();
        let _ = listener_task.await;
    }
    if !scanner_finished {
        scanner_task.abort();
        let _ = scanner_task.await;
    }
    for collector_task in collector_tasks {
        let _ = collector_task.await;
    }
    if paths.socket.exists() {
        fs::remove_file(&paths.socket)
            .with_context(|| format!("remove daemon socket {}", paths.socket.display()))?;
    }
    outcome
}

async fn bootstrap_scan(
    mut scanner: Scanner,
    scan_interval: Duration,
) -> Result<Option<(Scanner, Snapshot)>> {
    let mut missing_server = MissingServerPolicy::default();
    loop {
        let (next_scanner, result) = scan_once(scanner).await?;
        scanner = next_scanner;
        match result {
            Ok(snapshot) => {
                missing_server.success();
                return Ok(Some((scanner, snapshot)));
            }
            Err(error) => match missing_server.error(&error) {
                ScanErrorAction::Retry => tokio::time::sleep(scan_interval).await,
                ScanErrorAction::Exit => return Ok(None),
                ScanErrorAction::Report => return Err(error),
            },
        }
    }
}

async fn scan_once(scanner: Scanner) -> Result<(Scanner, Result<Snapshot>)> {
    tokio::task::spawn_blocking(move || {
        let mut scanner = scanner;
        let result = scanner.scan();
        (scanner, result)
    })
    .await
    .context("join local scanner")
}

async fn scan_loop(
    mut scanner: Scanner,
    shared: Arc<Shared>,
    state_path: PathBuf,
    scan_interval: Duration,
) -> Result<ScanLoopExit> {
    let mut interval = tokio::time::interval(scan_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut missing_server = MissingServerPolicy::default();
    loop {
        interval.tick().await;
        let (next_scanner, result) = scan_once(scanner).await?;
        scanner = next_scanner;
        match result {
            Ok(snapshot) => {
                missing_server.success();
                if shared.publish_local(snapshot).await
                    && let Err(error) = store::save(&state_path, &scanner.persisted())
                {
                    eprintln!("tmux-agent: persist state: {error:#}");
                }
            }
            Err(error) => match missing_server.error(&error) {
                ScanErrorAction::Retry => {}
                ScanErrorAction::Exit => return Ok(ScanLoopExit::ServerMissing),
                ScanErrorAction::Report => eprintln!("tmux-agent: scan failed: {error:#}"),
            },
        }
    }
}

async fn accept_clients(
    listener: UnixListener,
    shared: Arc<Shared>,
    shutdown: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await.context("accept daemon client")?;
        tokio::spawn(serve_client(stream, shared.clone(), shutdown.clone()));
    }
}

async fn serve_client(
    stream: UnixStream,
    shared: Arc<Shared>,
    shutdown: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let request: IpcRequest = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            write_response(
                &mut writer,
                &IpcResponse::Error {
                    message: format!("invalid request: {error}"),
                },
            )
            .await?;
            return Ok(());
        }
    };
    match request {
        IpcRequest::Snapshot { local_only } => {
            let snapshot = shared.snapshot(local_only).await;
            write_response(&mut writer, &IpcResponse::Snapshot { snapshot }).await?;
        }
        IpcRequest::Watch { local_only } => {
            let mut changed = if local_only {
                shared.local_changed.subscribe()
            } else {
                shared.changed.subscribe()
            };
            let mut disconnect_probe = [0_u8; 1];
            loop {
                let snapshot = shared.snapshot(local_only).await;
                if write_response(&mut writer, &IpcResponse::Snapshot { snapshot })
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::select! {
                    result = changed.changed() => {
                        if result.is_err() {
                            break;
                        }
                    }
                    // Watch clients send one request. Any later read completion
                    // means the client closed or violated that request boundary.
                    _ = reader.read(&mut disconnect_probe) => break,
                }
            }
        }
        IpcRequest::Acknowledge { target } => {
            if shared.acknowledge(&target).await? {
                write_response(&mut writer, &IpcResponse::Ack).await?;
            } else {
                write_response(
                    &mut writer,
                    &IpcResponse::Error {
                        message: format!("no agent matches {target:?}"),
                    },
                )
                .await?;
            }
        }
        IpcRequest::MarkUsed { target } => {
            if shared.mark_used(&target).await {
                write_response(&mut writer, &IpcResponse::Ack).await?;
            } else {
                write_response(
                    &mut writer,
                    &IpcResponse::Error {
                        message: format!("no top-level agent matches {target:?}"),
                    },
                )
                .await?;
            }
        }
        IpcRequest::Shutdown => {
            write_response(&mut writer, &IpcResponse::Ack).await?;
            let _ = shutdown.send(()).await;
        }
    }
    Ok(())
}

fn namespace_remote(alias: &str, snapshot: &mut Snapshot) {
    for agent in &mut snapshot.agents {
        if let Some(subagent) = &mut agent.subagent {
            subagent.parent_id = format!("remote/{alias}/{}", subagent.parent_id);
        }
        agent.id = format!("remote/{alias}/{}", agent.id);
        agent.host = alias.to_string();
        agent.remote_alias = Some(alias.to_string());
        agent.focus_target = None;
    }
}

enum UniqueTransport<'a> {
    None,
    One(&'a SshTransport),
    Ambiguous,
}

fn reconcile_transports(agents: &mut [AgentRecord], transports: &[SshTransport]) {
    let title_counts = agents
        .iter()
        .filter_map(|agent| {
            let alias = agent.remote_alias.as_ref()?;
            let title = crate::tmux::normalize_transport_title(&agent.title);
            (!title.is_empty()).then(|| ((alias.clone(), title), 1_usize))
        })
        .fold(HashMap::new(), |mut counts, (key, count)| {
            *counts.entry(key).or_insert(0) += count;
            counts
        });

    for agent in agents {
        match exact_transport(agent, transports) {
            UniqueTransport::One(transport) => {
                apply_transport(agent, transport, true);
                continue;
            }
            UniqueTransport::Ambiguous => continue,
            UniqueTransport::None => {}
        }

        let Some(alias) = agent.remote_alias.as_deref() else {
            continue;
        };
        if agent.is_tmux() {
            match unique_transport(transports.iter().filter(|transport| {
                transport.remote_host == alias
                    && transport.remote_host_explicit
                    && transport.remote_session.as_deref() == Some(agent.session_name.as_str())
            })) {
                UniqueTransport::One(transport) => {
                    apply_transport(agent, transport, false);
                    continue;
                }
                UniqueTransport::Ambiguous => continue,
                UniqueTransport::None => {}
            }
            continue;
        }

        let title = crate::tmux::normalize_transport_title(&agent.title);
        if title.is_empty() || title_counts.get(&(alias.to_string(), title.clone())) != Some(&1) {
            continue;
        }
        if let UniqueTransport::One(transport) =
            unique_transport(transports.iter().filter(|transport| {
                transport.remote_host == alias
                    && !transport.remote_host_explicit
                    && transport.remote_session.is_none()
                    && transport.title == title
            }))
        {
            apply_transport(agent, transport, false);
        }
    }
}

fn apply_transport(agent: &mut AgentRecord, transport: &SshTransport, exact_connection: bool) {
    if exact_connection {
        agent.focus_target = Some(transport.target.clone());
    }
    if let Some(label) = transport
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        agent.label = Some(label.to_string());
    }
}

fn exact_transport<'a>(agent: &AgentRecord, transports: &'a [SshTransport]) -> UniqueTransport<'a> {
    let Some(connection) = agent.ssh_connection.as_ref() else {
        return UniqueTransport::None;
    };
    unique_transport(
        transports
            .iter()
            .filter(|transport| transport.connection.as_ref() == Some(connection)),
    )
}

fn unique_transport<'a>(
    mut matches: impl Iterator<Item = &'a SshTransport>,
) -> UniqueTransport<'a> {
    let Some(transport) = matches.next() else {
        return UniqueTransport::None;
    };
    if matches.next().is_some() {
        UniqueTransport::Ambiguous
    } else {
        UniqueTransport::One(transport)
    }
}

fn apply_acknowledgements(
    agents: &mut [AgentRecord],
    acknowledgements: &mut Acknowledgements,
) -> bool {
    let original_completion_len = acknowledgements.completions.len();
    let original_goal_len = acknowledgements.goal_achievements.len();
    for agent in agents {
        let active = matches!(agent.state, AgentState::Working | AgentState::Blocked);
        let pursuing = agent
            .goal
            .is_some_and(|goal| goal.state == GoalState::Pursuing);
        let current_goal = agent
            .goal
            .as_ref()
            .filter(|goal| goal.state == GoalState::Achieved)
            .filter(|goal| goal.achievement_observed_at_ms > 0)
            .map(|goal| (goal.achievement_observed_at_ms, goal.achievement_pending));
        let acknowledged_goal = acknowledgements.goal_achievements.get(&agent.id).copied();
        if pursuing {
            if let Some(goal) = &mut agent.goal {
                goal.achievement_pending = false;
            }
            acknowledgements.completions.remove(&agent.id);
            acknowledgements.goal_achievements.remove(&agent.id);
            continue;
        }
        if active {
            let acknowledged_deferred_goal = current_goal.zip(acknowledged_goal).is_some_and(
                |((current, pending), acknowledged)| pending && current == acknowledged,
            );
            if acknowledged_deferred_goal {
                if let Some(goal) = &mut agent.goal {
                    goal.achievement_pending = false;
                }
            } else {
                acknowledgements.completions.remove(&agent.id);
                acknowledgements.goal_achievements.remove(&agent.id);
            }
            continue;
        }

        let new_goal = match (current_goal, acknowledged_goal) {
            (Some((current, _)), Some(acknowledged)) if current != acknowledged => true,
            (Some((_, true)), None) if acknowledgements.completions.contains(&agent.id) => true,
            _ => false,
        };
        if new_goal {
            acknowledgements.completions.remove(&agent.id);
            acknowledgements.goal_achievements.remove(&agent.id);
        } else if current_goal
            .zip(acknowledged_goal)
            .is_some_and(|((current, _), acknowledged)| current == acknowledged)
            && let Some(goal) = &mut agent.goal
        {
            goal.achievement_pending = false;
        }

        if acknowledgements.completions.contains(&agent.id) && agent.state == AgentState::Idle {
            agent.seen = true;
            agent.attention = Attention::Idle;
        }
    }
    acknowledgements.completions.len() != original_completion_len
        || acknowledgements.goal_achievements.len() != original_goal_len
}

#[derive(Clone, Copy, Debug, Default)]
struct AcknowledgeResult {
    found: bool,
    persist_completion: bool,
    goal_achievement: Option<u64>,
}

fn acknowledge_records(agents: &mut [AgentRecord], target: &str) -> AcknowledgeResult {
    let Some(agent) = agents.iter_mut().find(|agent| agent.id == target) else {
        return AcknowledgeResult::default();
    };
    let goal_achievement = agent
        .goal
        .filter(|goal| goal.achievement_pending && goal.achievement_observed_at_ms > 0)
        .map(|goal| goal.achievement_observed_at_ms);
    agent.seen = true;
    let idle = agent.state == AgentState::Idle;
    if idle {
        agent.attention = Attention::Idle;
    }
    if let Some(goal) = &mut agent.goal {
        goal.achievement_pending = false;
    }
    AcknowledgeResult {
        found: true,
        persist_completion: idle || goal_achievement.is_some(),
        goal_achievement,
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &IpcResponse,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(response).context("serialize daemon response")?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .context("write daemon response")
}

async fn remote_loop(
    remote: RemoteConfig,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let message = match stream_remote(&remote, shared.clone(), &mut shutdown).await {
            Ok(RemoteStreamExit::Ended) => "remote stream ended".to_string(),
            Ok(RemoteStreamExit::Shutdown) => return,
            Err(error) => concise_error(&error),
        };
        shared.peer_error(&remote.name, message).await;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

async fn stream_remote(
    remote: &RemoteConfig,
    shared: Arc<Shared>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<RemoteStreamExit> {
    let executable = remote.command.first().context("remote command is empty")?;
    let mut child = Command::new(executable)
        .args(&remote.command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start remote collector {}", remote.name))?;
    let stdout = child.stdout.take().context("capture remote stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    child.kill().await.context("stop remote collector")?;
                    return Ok(RemoteStreamExit::Shutdown);
                }
            }
            line = lines.next_line() => {
                let Some(line) = line.context("read remote stream")? else {
                    break;
                };
                let snapshot: Snapshot = serde_json::from_str(&line)
                    .with_context(|| format!("parse snapshot from {}", remote.name))?;
                validate_remote_snapshot(&remote.name, &snapshot)?;
                shared.publish_remote(&remote.name, snapshot).await;
            }
        }
    }
    let status = child.wait().await.context("wait for remote collector")?;
    if !status.success() {
        bail!("remote collector exited with {status}");
    }
    Ok(RemoteStreamExit::Ended)
}

fn validate_remote_snapshot(alias: &str, snapshot: &Snapshot) -> Result<()> {
    if snapshot.protocol != PROTOCOL_VERSION {
        bail!(
            "remote {0} uses protocol {1}; tmux-agent {2} requires protocol {3}. Update tmux-agent on {0}",
            alias,
            snapshot.protocol,
            APPLICATION_VERSION,
            PROTOCOL_VERSION
        );
    }
    Ok(())
}

fn concise_error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}").replace('\n', " ");
    message.chars().take(240).collect()
}

pub async fn ensure_running(config_path: &Path, paths: &RuntimePaths) -> Result<()> {
    if UnixStream::connect(&paths.socket).await.is_ok() {
        return Ok(());
    }
    paths.ensure_dirs()?;
    let executable = std::env::current_exe().context("locate tmux-agent executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)
        .with_context(|| format!("open daemon log {}", paths.log.display()))?;
    let stderr = log.try_clone().context("clone daemon log handle")?;
    let mut command = std::process::Command::new(executable);
    command
        .args(["--config", &config_path.to_string_lossy(), "daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    // SAFETY: pre_exec calls only async-signal-safe setsid and error retrieval.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("start tmux-agent daemon")?;

    for _ in 0..30 {
        if UnixStream::connect(&paths.socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "daemon did not start; inspect {}",
        paths.log.to_string_lossy()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::SnapshotWatch;
    use crate::model::{
        AgentOrigin, EvidenceSource, GoalInfo, SshConnection, SshTransport, TmuxTarget,
    };
    use tempfile::tempdir;

    fn agent(id: &str, state: AgentState, attention: Attention) -> AgentRecord {
        AgentRecord {
            id: id.into(),
            host: "shared-host".into(),
            server: "default".into(),
            pane_id: "%1".into(),
            pane_pid: 10,
            session_id: "$1".into(),
            session_name: "main".into(),
            window_id: "@1".into(),
            window_index: 1,
            window_name: "work".into(),
            pane_index: 0,
            agent: "Codex".into(),
            state,
            attention,
            source: EvidenceSource::Screen,
            title: "work".into(),
            label: None,
            cwd: "/tmp".into(),
            visible: false,
            seen: attention != Attention::Done,
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

    fn transport(
        session: &str,
        connection: Option<SshConnection>,
        title: &str,
        label: Option<&str>,
    ) -> SshTransport {
        SshTransport {
            connection,
            remote_host: "remote-mac".into(),
            remote_host_explicit: false,
            remote_session: None,
            title: title.into(),
            label: label.map(str::to_string),
            target: TmuxTarget {
                session_name: session.into(),
                window_id: format!("@{session}"),
                window_index: 1,
                pane_id: format!("%{session}"),
                pane_index: 0,
            },
        }
    }

    #[tokio::test]
    async fn dropping_snapshot_watch_ends_the_daemon_client_task_without_a_model_change() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let acknowledgements_path = directory.path().join("acknowledged.json");
        let initial = Snapshot {
            revision: 1,
            agents: vec![agent(
                "shared-host/default/%1",
                AgentState::Idle,
                Attention::Idle,
            )],
            ..Snapshot::default()
        };
        let shared = Shared::new(
            initial.clone(),
            &[],
            Acknowledgements::default(),
            acknowledgements_path,
        )
        .unwrap();
        let server_shared = shared.clone();
        let (shutdown, _) = tokio::sync::mpsc::channel(1);
        let mut server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, server_shared, shutdown).await
        });

        let mut watch = SnapshotWatch::connect(&socket, false).await.unwrap();
        assert_eq!(watch.next_snapshot().await.unwrap().unwrap().revision, 1);

        let mut changed = initial;
        changed.revision = 2;
        changed.agents[0].title = "changed".into();
        assert!(shared.publish_local(changed).await);
        assert_eq!(watch.next_snapshot().await.unwrap().unwrap().revision, 2);

        drop(watch);
        let completed = tokio::time::timeout(Duration::from_millis(500), &mut server).await;
        if completed.is_err() {
            server.abort();
        }
        completed
            .expect("daemon watch task should stop when its client disconnects")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn successful_use_reorders_idle_agents_for_every_local_ui_watch() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut alpha = agent("shared-host/default/%1", AgentState::Idle, Attention::Idle);
        alpha.session_name = "alpha".into();
        let mut beta = agent("shared-host/default/%2", AgentState::Idle, Attention::Idle);
        beta.session_name = "beta".into();
        let shared = Shared::new(
            Snapshot {
                revision: 1,
                agents: vec![alpha, beta],
                ..Snapshot::default()
            },
            &[],
            Acknowledgements::default(),
            directory.path().join("acknowledged.json"),
        )
        .unwrap();
        let (shutdown, _) = tokio::sync::mpsc::channel(1);
        let server = tokio::spawn(accept_clients(listener, shared.clone(), shutdown));
        let mut first_watch = SnapshotWatch::connect(&socket, false).await.unwrap();
        let mut second_watch = SnapshotWatch::connect(&socket, false).await.unwrap();

        for watch in [&mut first_watch, &mut second_watch] {
            let initial = watch.next_snapshot().await.unwrap().unwrap();
            assert_eq!(
                initial
                    .agents
                    .iter()
                    .map(|agent| agent.session_name.as_str())
                    .collect::<Vec<_>>(),
                ["alpha", "beta"]
            );
        }

        crate::ipc::mark_used(&socket, "shared-host/default/%2")
            .await
            .unwrap();

        for watch in [&mut first_watch, &mut second_watch] {
            let changed = tokio::time::timeout(Duration::from_secs(1), watch.next_snapshot())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                changed
                    .agents
                    .iter()
                    .map(|agent| agent.session_name.as_str())
                    .collect::<Vec<_>>(),
                ["beta", "alpha"]
            );
            let encoded = serde_json::to_value(changed).unwrap();
            assert!(!encoded.to_string().contains("last_used"));
        }

        let federated = crate::ipc::snapshot(&socket, true).await.unwrap();
        assert_eq!(
            federated
                .agents
                .iter()
                .map(|agent| agent.session_name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );

        let mut without_beta = federated.clone();
        without_beta.revision = 2;
        without_beta
            .agents
            .retain(|agent| agent.session_name == "alpha");
        assert!(shared.publish_local(without_beta).await);
        let mut restored = federated;
        restored.revision = 3;
        assert!(shared.publish_local(restored).await);
        assert_eq!(
            shared
                .snapshot(false)
                .await
                .agents
                .iter()
                .map(|agent| agent.session_name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );

        server.abort();
    }

    #[test]
    fn remote_ids_and_display_hosts_use_configured_alias() {
        let mut snapshot = Snapshot {
            protocol: PROTOCOL_VERSION,
            application_version: Some(APPLICATION_VERSION.to_string()),
            capabilities: crate::model::application_capabilities(),
            revision: 1,
            host: "shared-host".into(),
            server: "default".into(),
            generated_at_ms: 1,
            agents: vec![agent(
                "shared-host/default/%1",
                AgentState::Idle,
                Attention::Idle,
            )],
            peers: Vec::new(),
            ssh_transports: Vec::new(),
        };
        namespace_remote("remote-mac", &mut snapshot);
        assert_eq!(
            snapshot.agents[0].id,
            "remote/remote-mac/shared-host/default/%1"
        );
        assert_eq!(snapshot.agents[0].host, "remote-mac");
        assert_eq!(
            snapshot.agents[0].remote_alias.as_deref(),
            Some("remote-mac")
        );
    }

    #[test]
    fn remote_subagent_parent_ids_use_the_same_namespace() {
        let parent = agent(
            "shared-host/default/%1",
            AgentState::Working,
            Attention::Working,
        );
        let mut child = agent(
            "shared-host/terminal/ttys002/70",
            AgentState::Unknown,
            Attention::Unknown,
        );
        child.subagent = Some(crate::model::SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut snapshot = Snapshot {
            agents: vec![parent, child],
            ..Snapshot::default()
        };

        namespace_remote("remote-mac", &mut snapshot);

        assert_eq!(
            snapshot.agents[1]
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str()),
            Some("remote/remote-mac/shared-host/default/%1")
        );
    }

    #[test]
    fn acknowledgement_is_applied_until_the_next_active_turn() {
        let id = "remote/remote-mac/session";
        let mut agents = vec![agent(id, AgentState::Idle, Attention::Done)];
        let mut acknowledgements = Acknowledgements {
            completions: HashSet::from([id.to_string()]),
            ..Acknowledgements::default()
        };
        apply_acknowledgements(&mut agents, &mut acknowledgements);
        assert!(agents[0].seen);
        assert_eq!(agents[0].attention, Attention::Idle);

        agents[0].state = AgentState::Working;
        agents[0].attention = Attention::Working;
        apply_acknowledgements(&mut agents, &mut acknowledgements);
        assert!(acknowledgements.completions.is_empty());
    }

    #[tokio::test]
    async fn loaded_acknowledgement_hides_goal_in_initial_snapshot() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("acknowledged.json");
        let id = "shared-host/default/%1";
        let state = AcknowledgedState {
            protocol: PROTOCOL_VERSION,
            ids: vec![id.into()],
            goal_achievements: vec![GoalAcknowledgement {
                id: id.into(),
                achievement_observed_at_ms: 123_000,
            }],
        };
        store::save_acknowledged(&path, &state).unwrap();
        let acknowledgements =
            Acknowledgements::from_state(store::load_acknowledged(&path).unwrap());
        let mut completed = agent(id, AgentState::Idle, Attention::Done);
        completed.goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        });
        let snapshot = Snapshot {
            agents: vec![completed],
            ..Snapshot::default()
        };

        let shared = Shared::new(snapshot, &[], acknowledgements, path).unwrap();
        let inner = shared.inner.read().await;

        assert_eq!(inner.local.agents[0].attention, Attention::Idle);
        assert!(
            !inner.local.agents[0]
                .goal
                .as_ref()
                .unwrap()
                .achievement_pending
        );
    }

    #[tokio::test]
    async fn transport_label_change_publishes_a_new_local_snapshot() {
        let directory = tempdir().unwrap();
        let mut local = Snapshot {
            protocol: PROTOCOL_VERSION,
            application_version: Some(APPLICATION_VERSION.to_string()),
            revision: 1,
            host: "local".into(),
            server: "default".into(),
            ssh_transports: vec![transport("transport", None, "work", None)],
            ..Snapshot::default()
        };
        let shared = Shared::new(
            local.clone(),
            &[],
            Acknowledgements::default(),
            directory.path().join("acknowledged.json"),
        )
        .unwrap();
        local.revision = 2;
        local.ssh_transports[0].label = Some("testing env".into());

        assert!(shared.publish_local(local.clone()).await);
        assert!(!shared.publish_local(local).await);
    }

    #[test]
    fn acknowledgement_survives_unknown_evidence() {
        let id = "remote/remote-mac/session";
        let mut agents = vec![agent(id, AgentState::Unknown, Attention::Unknown)];
        agents[0].goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        });
        let mut acknowledgements = Acknowledgements {
            completions: HashSet::from([id.to_string()]),
            goal_achievements: HashMap::from([(id.to_string(), 123_000)]),
        };
        apply_acknowledgements(&mut agents, &mut acknowledgements);
        assert!(acknowledgements.completions.contains(id));
        assert!(!agents[0].goal.as_ref().unwrap().achievement_pending);

        agents[0].state = AgentState::Idle;
        agents[0].attention = Attention::Done;
        apply_acknowledgements(&mut agents, &mut acknowledgements);
        assert_eq!(agents[0].attention, Attention::Idle);
    }

    #[test]
    fn acknowledging_unknown_agent_persists_a_pending_goal_notice() {
        let mut agents = vec![agent(
            "remote/remote-mac/session",
            AgentState::Unknown,
            Attention::Unknown,
        )];
        agents[0].goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        });

        let result = acknowledge_records(&mut agents, "remote/remote-mac/session");

        assert!(result.found);
        assert!(result.persist_completion);
        assert_eq!(result.goal_achievement, Some(123_000));
        assert!(!agents[0].goal.as_ref().unwrap().achievement_pending);
    }

    #[test]
    fn acknowledgement_suppresses_deferred_goal_through_active_finish_and_idle() {
        let id = "remote/remote-mac/session";
        let mut agents = vec![agent(id, AgentState::Working, Attention::Working)];
        agents[0].goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        });
        let mut acknowledgements = Acknowledgements {
            completions: HashSet::from([id.to_string()]),
            goal_achievements: HashMap::from([(id.to_string(), 123_000)]),
        };

        apply_acknowledgements(&mut agents, &mut acknowledgements);
        assert!(acknowledgements.completions.contains(id));
        assert_eq!(acknowledgements.goal_achievements.get(id), Some(&123_000));
        assert!(!agents[0].goal.as_ref().unwrap().achievement_pending);

        agents[0].goal.as_mut().unwrap().achievement_pending = true;
        apply_acknowledgements(&mut agents, &mut acknowledgements);
        assert!(!agents[0].goal.as_ref().unwrap().achievement_pending);

        agents[0].state = AgentState::Idle;
        agents[0].attention = Attention::Done;
        agents[0].goal.as_mut().unwrap().achievement_pending = true;
        apply_acknowledgements(&mut agents, &mut acknowledgements);
        assert_eq!(agents[0].attention, Attention::Idle);
        assert!(!agents[0].goal.as_ref().unwrap().achievement_pending);
    }

    #[test]
    fn new_active_turn_clears_acknowledgement_for_retired_goal_notice() {
        let id = "remote/remote-mac/session";
        let mut agents = vec![agent(id, AgentState::Working, Attention::Working)];
        agents[0].goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: false,
            achievement_observed_at_ms: 123_000,
        });
        let mut acknowledgements = Acknowledgements {
            completions: HashSet::from([id.to_string()]),
            goal_achievements: HashMap::from([(id.to_string(), 123_000)]),
        };

        apply_acknowledgements(&mut agents, &mut acknowledgements);

        assert!(acknowledgements.completions.is_empty());
        assert!(acknowledgements.goal_achievements.is_empty());
    }

    #[test]
    fn pursuing_goal_clears_the_previous_acknowledgement() {
        let mut agents = vec![agent(
            "remote/remote-mac/session",
            AgentState::Idle,
            Attention::Idle,
        )];
        agents[0].goal = Some(GoalInfo {
            state: GoalState::Pursuing,
            elapsed_seconds: 5,
            achievement_pending: false,
            achievement_observed_at_ms: 0,
        });
        let mut acknowledgements = Acknowledgements {
            completions: HashSet::from(["remote/remote-mac/session".to_string()]),
            goal_achievements: HashMap::from([("remote/remote-mac/session".to_string(), 123_000)]),
        };

        apply_acknowledgements(&mut agents, &mut acknowledgements);

        assert!(acknowledgements.completions.is_empty());
        assert!(acknowledgements.goal_achievements.is_empty());
    }

    #[test]
    fn reconnecting_with_new_goal_does_not_reuse_stale_acknowledgement() {
        let id = "remote/remote-mac/session";
        let mut agents = vec![agent(id, AgentState::Idle, Attention::Done)];
        agents[0].goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 42,
            achievement_pending: true,
            achievement_observed_at_ms: 456_000,
        });
        let mut acknowledgements = Acknowledgements {
            completions: HashSet::from([id.to_string()]),
            goal_achievements: HashMap::from([(id.to_string(), 123_000)]),
        };

        apply_acknowledgements(&mut agents, &mut acknowledgements);

        assert!(agents[0].goal.as_ref().unwrap().achievement_pending);
        assert_eq!(agents[0].attention, Attention::Done);
        assert!(acknowledgements.completions.is_empty());
        assert!(acknowledgements.goal_achievements.is_empty());
    }

    #[test]
    fn ssh_connection_resolves_to_unique_local_tmux_target() {
        let connection = SshConnection {
            client_address: "192.0.2.10".into(),
            client_port: 64308,
            server_address: "198.51.100.20".into(),
            server_port: 22,
        };
        let target = TmuxTarget {
            session_name: "workspace".into(),
            window_id: "@57".into(),
            window_index: 1,
            pane_id: "%57".into(),
            pane_index: 0,
        };
        let mut record = agent(
            "remote/remote-mac/terminal/ttys004/26621",
            AgentState::Unknown,
            Attention::Unknown,
        );
        record.ssh_connection = Some(connection.clone());
        let mut transport = transport("workspace", Some(connection), "work", None);
        transport.target = target.clone();
        let transports = [transport];
        let UniqueTransport::One(resolved) = exact_transport(&record, &transports) else {
            panic!("expected one exact transport");
        };
        assert_eq!(resolved.target, target);
    }

    #[test]
    fn duplicate_socket_matches_are_not_guessed() {
        let connection = SshConnection {
            client_address: "192.0.2.10".into(),
            client_port: 64308,
            server_address: "198.51.100.20".into(),
            server_port: 22,
        };
        let mut record = agent(
            "remote/remote-mac/terminal/ttys004/26621",
            AgentState::Unknown,
            Attention::Unknown,
        );
        record.ssh_connection = Some(connection.clone());
        let transports = ["one", "two"]
            .map(|session| transport(session, Some(connection.clone()), "work", None));
        assert!(matches!(
            exact_transport(&record, &transports),
            UniqueTransport::Ambiguous
        ));
    }

    #[test]
    fn exact_transport_applies_local_label_and_focus_target() {
        let connection = SshConnection {
            client_address: "192.0.2.10".into(),
            client_port: 64308,
            server_address: "198.51.100.20".into(),
            server_port: 22,
        };
        let mut record = agent(
            "remote/remote-mac/terminal/ttys004/26621",
            AgentState::Working,
            Attention::Working,
        );
        record.remote_alias = Some("remote-mac".into());
        record.ssh_connection = Some(connection.clone());
        record.label = Some("remote label".into());
        let transports = [transport(
            "local-transport",
            Some(connection),
            "work",
            Some("  testing env  "),
        )];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert_eq!(record.label.as_deref(), Some("testing env"));
        assert_eq!(
            record
                .focus_target
                .as_ref()
                .map(|target| target.pane_id.as_str()),
            Some("%local-transport")
        );
    }

    #[test]
    fn empty_local_transport_label_preserves_remote_label() {
        let connection = SshConnection {
            client_address: "192.0.2.10".into(),
            client_port: 64308,
            server_address: "198.51.100.20".into(),
            server_port: 22,
        };
        let mut record = agent(
            "remote/remote-mac/terminal/ttys004/26621",
            AgentState::Idle,
            Attention::Idle,
        );
        record.ssh_connection = Some(connection.clone());
        record.label = Some("remote label".into());
        let transports = [transport(
            "transport",
            Some(connection),
            "work",
            Some("   "),
        )];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert_eq!(record.label.as_deref(), Some("remote label"));
        assert!(record.focus_target.is_some());
    }

    #[test]
    fn unique_remote_terminal_title_applies_label_without_changing_focus_resolution() {
        let mut record = agent(
            "remote/remote-mac/default/%4",
            AgentState::Working,
            Attention::Working,
        );
        record.origin = AgentOrigin::Terminal;
        record.remote_alias = Some("remote-mac".into());
        record.title = "⠦ project-one".into();
        let transports = [transport(
            "transport",
            None,
            "project-one",
            Some("integration"),
        )];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert_eq!(record.label.as_deref(), Some("integration"));
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn unique_remote_session_marker_applies_label_after_title_miss() {
        let mut record = agent(
            "remote/remote-mac/default/%4",
            AgentState::Idle,
            Attention::Idle,
        );
        record.remote_alias = Some("remote-mac".into());
        record.session_name = "remote-work".into();
        record.title = "agent-title".into();
        let mut marked = transport("transport", None, "different-title", Some("manual"));
        marked.remote_host_explicit = true;
        marked.remote_session = Some("remote-work".into());
        let transports = [marked];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert_eq!(record.label.as_deref(), Some("manual"));
        assert_eq!(record.location_label(), "remote-work:1.0");
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn ambiguous_remote_title_does_not_apply_a_sibling_label() {
        let mut record = agent(
            "remote/remote-mac/default/%4",
            AgentState::Working,
            Attention::Working,
        );
        record.origin = AgentOrigin::Terminal;
        record.remote_alias = Some("remote-mac".into());
        record.title = "same-project".into();
        let transports =
            ["one", "two"].map(|session| transport(session, None, "same-project", Some(session)));

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert!(record.label.is_none());
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn conflicting_session_marker_cannot_match_by_title() {
        let mut record = agent(
            "remote/remote-mac/default/%4",
            AgentState::Working,
            Attention::Working,
        );
        record.remote_alias = Some("remote-mac".into());
        record.session_name = "session-b".into();
        record.title = "same-project".into();
        let mut marked = transport("transport", None, "same-project", Some("wrong"));
        marked.remote_host_explicit = true;
        marked.remote_session = Some("session-a".into());
        let transports = [marked];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert!(record.label.is_none());
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn host_only_marker_is_not_used_for_terminal_title_fallback() {
        let mut record = agent(
            "remote/remote-mac/terminal/ttys004/26621",
            AgentState::Working,
            Attention::Working,
        );
        record.origin = AgentOrigin::Terminal;
        record.remote_alias = Some("remote-mac".into());
        record.title = "same-project".into();
        let mut partial = transport("host-only", None, "same-project", Some("wrong"));
        partial.remote_host_explicit = true;
        let transports = [partial];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert!(record.label.is_none());
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn session_only_marker_is_not_a_complete_remote_tmux_mapping() {
        let mut record = agent(
            "remote/remote-mac/default/%4",
            AgentState::Working,
            Attention::Working,
        );
        record.remote_alias = Some("remote-mac".into());
        record.session_name = "remote-work".into();
        record.title = "same-project".into();
        let mut partial = transport("session-only", None, "same-project", Some("wrong"));
        partial.remote_session = Some("remote-work".into());
        let transports = [partial];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert!(record.label.is_none());
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn explicit_session_marker_precedes_unmarked_title_match() {
        let mut record = agent(
            "remote/remote-mac/default/%4",
            AgentState::Working,
            Attention::Working,
        );
        record.remote_alias = Some("remote-mac".into());
        record.session_name = "remote-work".into();
        record.title = "same-project".into();
        let mut marked = transport("marked", None, "other-title", Some("session label"));
        marked.remote_host_explicit = true;
        marked.remote_session = Some("remote-work".into());
        let unmarked = transport("title", None, "same-project", Some("title label"));
        let transports = [marked, unmarked];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert_eq!(record.label.as_deref(), Some("session label"));
        assert_eq!(record.location_label(), "remote-work:1.0");
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn unmarked_remote_tmux_agent_does_not_use_a_sibling_title_transport() {
        let mut record = agent(
            "remote/remote-mac/default/%4",
            AgentState::Working,
            Attention::Working,
        );
        record.remote_alias = Some("remote-mac".into());
        record.session_name = "wmtc-manual-48".into();
        record.title = "walk-me-through-the-code".into();
        let transports = [transport(
            "walkme-1.1",
            None,
            "walk-me-through-the-code",
            Some("development"),
        )];

        reconcile_transports(std::slice::from_mut(&mut record), &transports);

        assert!(record.label.is_none());
        assert!(record.focus_target.is_none());
    }

    #[test]
    fn one_title_transport_does_not_label_two_remote_agents() {
        let mut agents = ["%4", "%5"].map(|pane| {
            let mut record = agent(
                &format!("remote/remote-mac/default/{pane}"),
                AgentState::Working,
                Attention::Working,
            );
            record.origin = AgentOrigin::Terminal;
            record.remote_alias = Some("remote-mac".into());
            record.title = "same-project".into();
            record
        });
        let transports = [transport(
            "transport",
            None,
            "same-project",
            Some("shared label"),
        )];

        reconcile_transports(&mut agents, &transports);

        assert!(agents.iter().all(|agent| agent.label.is_none()));
        assert!(agents.iter().all(|agent| agent.focus_target.is_none()));
    }

    #[test]
    fn incompatible_remote_protocol_error_names_the_peer_and_expected_protocol() {
        let snapshot = Snapshot {
            protocol: PROTOCOL_VERSION + 1,
            application_version: Some("9.0.0".into()),
            ..Snapshot::default()
        };
        let error = validate_remote_snapshot("remote-mac", &snapshot).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("remote-mac"));
        assert!(message.contains(&format!("protocol {}", PROTOCOL_VERSION + 1)));
        assert!(message.contains(&format!("requires protocol {PROTOCOL_VERSION}")));
        assert!(message.contains("Update"));
    }
}
