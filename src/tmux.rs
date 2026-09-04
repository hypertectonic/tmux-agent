use crate::config::{Config, shell_join};
use crate::model::{
    AgentRecord, ClientConnection, MoshEndpoint, SessionConnections, SshConnection, SshTransport,
    TmuxTarget, terminal_safe, trim_braille_activity_prefix,
};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::env;
#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SEPARATOR: char = '\u{1f}';
const ESCAPED_SEPARATOR: &str = r"\037";
const UI_SELECTION_OPTION: &str = "@tmux_agent_selection";
const REMOTE_HOST_OPTION: &str = "@tmux_agent_remote_host";
const REMOTE_SESSION_OPTION: &str = "@tmux_agent_remote_session";
const PROCESS_INVENTORY_TTL: Duration = Duration::from_secs(1);
const REMOTE_ATTACH_TIMEOUT: Duration = Duration::from_secs(2);
const REMOTE_ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(100);
static CAPTURE_BATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
const PROCESS_COLUMNS: &str = "uid=,pid=,ppid=,pgid=,tpgid=,tdev=,etime=,args=";
#[cfg(not(target_os = "macos"))]
const PROCESS_COLUMNS: &str = "uid=,pid=,ppid=,pgid=,tpgid=,tty=,etime=,args=";
#[cfg(target_os = "macos")]
static DEVNAME_LOCK: Mutex<()> = Mutex::new(());
// CSI 34~ is an otherwise unused F17 key that wakes every persistent UI after
// an explicit numeric selection without introducing polling or focus tracking.
const UI_SELECTION_WAKE_HEX: [&str; 5] = ["1b", "5b", "33", "34", "7e"];

fn selection_broadcast_panes(panes: &[Pane]) -> Vec<String> {
    panes
        .iter()
        .filter(|pane| pane.is_agent_ui && !pane.dead)
        .map(|pane| pane.pane_id.clone())
        .collect()
}

fn wake_ui_panes<F>(pane_ids: &[String], wake: F) -> Result<()>
where
    F: Fn(&str) -> Result<()> + Sync,
{
    let results = std::thread::scope(|scope| {
        pane_ids
            .iter()
            .map(|pane_id| scope.spawn(|| wake(pane_id)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|task| task.join().expect("UI wake worker panicked"))
            .collect::<Vec<_>>()
    });
    let mut first_error = None;
    for (pane_id, result) in pane_ids.iter().zip(results) {
        if let Err(error) = result
            && first_error.is_none()
        {
            first_error = Some(error.context(format!("wake tmux-agent UI pane {pane_id}")));
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[derive(Debug, Clone)]
pub struct Pane {
    pub pane_id: String,
    pub pane_pid: u32,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_index: u32,
    pub current_command: String,
    pub title: String,
    pub label: Option<String>,
    pub cwd: String,
    pub visible: bool,
    pub dead: bool,
    pub is_agent_ui: bool,
    pub mirror_host: Option<String>,
    pub mirror_session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePaneBinding {
    pub pane_id: String,
    pub remote: String,
    pub session: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusOutcome {
    Exact,
    TransportOnly,
}

pub const TRANSPORT_ONLY_FOCUS_MESSAGE: &str =
    "focused remote transport only; inner target not selected or verified";

#[derive(Debug, Clone)]
struct Process {
    uid: u32,
    pid: u32,
    parent_pid: u32,
    process_group: u32,
    foreground_group: Option<u32>,
    terminal: Option<String>,
    args: String,
}

#[derive(Debug, Clone)]
pub struct TerminalJob {
    pub name: String,
    pub process_group: u32,
    pub leader_pid: u32,
    pub pids: Vec<u32>,
    pub processes: String,
}

#[derive(Debug, Clone)]
pub struct ProcessSnapshot {
    pub panes: HashMap<String, String>,
    pub pane_groups: HashMap<String, u32>,
    pub pane_pids: HashMap<String, Vec<u32>>,
    pub process_args: HashMap<u32, String>,
    pub process_started_at_ms: HashMap<u32, u64>,
    pub terminals: Vec<TerminalJob>,
    pub live_pids: HashSet<u32>,
    pub parent_pids: HashMap<u32, u32>,
    pub ssh_connections: HashMap<u32, SshConnection>,
    pub client_connections: HashMap<u32, ClientConnection>,
    pub ssh_transports: Vec<SshTransport>,
}

#[derive(Debug)]
struct ProcessInventory {
    processes: Vec<Process>,
    process_started_at_ms: HashMap<u32, u64>,
    tcp_connections: HashMap<u32, Vec<TcpSocket>>,
    parent_pids: HashMap<u32, u32>,
    ssh_connections: HashMap<u32, SshConnection>,
    client_connections: HashMap<u32, ClientConnection>,
}

#[derive(Debug)]
struct CachedProcessInventory {
    refreshed_at: Instant,
    inventory: Arc<ProcessInventory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TcpEndpoint {
    address: String,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TcpConnection {
    left: TcpEndpoint,
    right: TcpEndpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TcpSocket {
    fd: u32,
    connection: TcpConnection,
}

#[derive(Debug, Clone)]
pub struct Tmux {
    args: Vec<String>,
    host_aliases: HashMap<String, String>,
    process_inventory: Arc<Mutex<Option<CachedProcessInventory>>>,
    #[cfg(target_os = "macos")]
    terminal_names: Arc<MacosTerminalNames>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct MacosTerminalNames {
    names: Mutex<HashMap<String, Option<String>>>,
}

#[cfg(target_os = "macos")]
impl MacosTerminalNames {
    fn resolve_with<F>(&self, value: &str, lookup: F) -> Option<String>
    where
        F: FnOnce(libc::dev_t) -> Option<String>,
    {
        let mut names = self
            .names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(name) = names.get(value) {
            return name.clone();
        }
        let name = resolve_macos_terminal_with(value, lookup);
        names.insert(value.to_string(), name.clone());
        name
    }
}

#[derive(Debug)]
struct FocusTargetMissing {
    alias: String,
    title: String,
    session: Option<String>,
    live_session: bool,
}

impl fmt::Display for FocusTargetMissing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(session) = &self.session {
            let alias = terminal_safe(&self.alias);
            let session = terminal_safe(session);
            if self.live_session {
                return write!(
                    formatter,
                    "no live local transport for {alias}/{session}; attach this remote session, then refresh. Translated or proxied endpoints may not support automatic association"
                );
            }
            return write!(
                formatter,
                "no local pane is bound to {}/{}; run tmux-agent remote bind {} {} --pane <local-pane-id> on this machine",
                alias, session, alias, session
            );
        }
        write!(
            formatter,
            "no unique local SSH or mosh pane for {} with title {:?}",
            self.alias, self.title
        )
    }
}

impl std::error::Error for FocusTargetMissing {}

pub fn is_focus_target_missing(error: &anyhow::Error) -> bool {
    error.downcast_ref::<FocusTargetMissing>().is_some()
}

#[derive(Debug)]
struct ServerMissing;

impl fmt::Display for ServerMissing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("tmux server is missing")
    }
}

impl std::error::Error for ServerMissing {}

pub fn is_server_missing(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ServerMissing>().is_some()
}

impl Tmux {
    pub fn new(config: &Config) -> Self {
        Self {
            args: config.tmux_args.clone(),
            host_aliases: configured_host_aliases(config),
            process_inventory: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "macos")]
            terminal_names: Arc::new(MacosTerminalNames::default()),
        }
    }

    pub fn server_key(&self) -> Result<Option<String>> {
        let Some(value) = self.run_optional(&["display-message", "-p", "#{socket_path}"])? else {
            return Ok(None);
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            Ok(Some("default".to_string()))
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    pub fn runtime_key(&self) -> String {
        let tmux_tmpdir = env::var_os("TMUX_TMPDIR");
        runtime_key_for(
            &self.args,
            env::var("TMUX").ok().as_deref(),
            tmux_tmpdir.as_deref(),
            unsafe { libc::geteuid() },
        )
    }

    pub fn list_panes(&self) -> Result<Vec<Pane>> {
        let sep = SEPARATOR.to_string();
        let format = [
            "#{pane_id}",
            "#{pane_pid}",
            "#{session_id}",
            "#{session_name}",
            "#{window_id}",
            "#{window_index}",
            "#{window_name}",
            "#{pane_index}",
            "#{pane_current_command}",
            "#{pane_title}",
            "#{pane_current_path}",
            "#{pane_dead}",
            "#{pane_active}",
            "#{window_active}",
            "#{session_attached}",
            "#{@tmux_agent_ui}",
            "#{@tmux_agent_remote_host}",
            "#{@tmux_agent_remote_session}",
            "#{@pane_label}",
        ]
        .join(&sep);
        let Some(output) = self.run_optional(&["list-panes", "-a", "-F", &format])? else {
            return Err(ServerMissing.into());
        };
        output.lines().map(parse_pane).collect()
    }

    pub fn process_snapshot(&self, panes: &[Pane]) -> Result<ProcessSnapshot> {
        self.process_snapshot_with(panes, Instant::now, || self.refresh_process_inventory())
    }

    pub fn session_connections(
        &self,
        processes: &ProcessSnapshot,
    ) -> Result<HashMap<String, SessionConnections>> {
        let format = [
            "#{session_id}",
            "#{session_created}",
            "#{pid}",
            "#{start_time}",
        ]
        .join(&SEPARATOR.to_string());
        let sessions = self
            .run_optional(&["list-sessions", "-F", &format])?
            .unwrap_or_default();
        let mut result = HashMap::new();
        for line in sessions.lines() {
            let line = line.replace(ESCAPED_SEPARATOR, &SEPARATOR.to_string());
            let fields = line.split(SEPARATOR).collect::<Vec<_>>();
            if let [id, created, pid, started] = fields.as_slice() {
                result.insert(
                    (*id).to_string(),
                    SessionConnections {
                        server_pid: parse_number(pid, "server pid")?,
                        server_started_at: parse_number(started, "server start")?,
                        session_created_at: parse_number(created, "session creation")?,
                        complete: true,
                        clients: Vec::new(),
                    },
                );
            } else {
                bail!("tmux did not report session attachment identity");
            }
        }
        let format = ["#{session_id}", "#{client_pid}"].join(&SEPARATOR.to_string());
        let clients = self
            .run_optional(&["list-clients", "-F", &format])?
            .unwrap_or_default();
        for line in clients.lines() {
            let line = line.replace(ESCAPED_SEPARATOR, &SEPARATOR.to_string());
            let Some((session, pid)) = line.split_once(SEPARATOR) else {
                bail!("tmux did not report client attachment identity");
            };
            let Some(session) = result.get_mut(session) else {
                continue;
            };
            match pid
                .parse()
                .ok()
                .and_then(|pid| processes.client_connections.get(&pid))
            {
                Some(connection) => session.clients.push(connection.clone()),
                None => session.complete = false,
            }
        }
        Ok(result)
    }

    fn process_snapshot_with<N, F>(
        &self,
        panes: &[Pane],
        mut now: N,
        refresh: F,
    ) -> Result<ProcessSnapshot>
    where
        N: FnMut() -> Instant,
        F: FnOnce() -> Result<ProcessInventory>,
    {
        let inventory = {
            let mut cached = self
                .process_inventory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let checked_at = now();
            if let Some(inventory) = cached
                .as_ref()
                .filter(|cached| {
                    checked_at.saturating_duration_since(cached.refreshed_at)
                        < PROCESS_INVENTORY_TTL
                })
                .map(|cached| Arc::clone(&cached.inventory))
            {
                inventory
            } else {
                let inventory = Arc::new(refresh()?);
                *cached = Some(CachedProcessInventory {
                    refreshed_at: now(),
                    inventory: Arc::clone(&inventory),
                });
                inventory
            }
        };
        Ok(self.project_process_snapshot(panes, &inventory))
    }

    fn refresh_process_inventory(&self) -> Result<ProcessInventory> {
        let output = Command::new("ps")
            .args(["-axww", "-o", PROCESS_COLUMNS])
            .output()
            .context("run ps for foreground process discovery")?;
        if !output.status.success() {
            bail!("ps failed with {}", output.status);
        }
        let process_output = String::from_utf8_lossy(&output.stdout);
        let process_started_at_ms = process_output
            .lines()
            .filter_map(parse_process_start)
            .collect::<HashMap<_, _>>();
        #[cfg(target_os = "macos")]
        let processes = parse_processes(&process_output, &self.terminal_names);
        #[cfg(not(target_os = "macos"))]
        let processes = parse_processes(&process_output);
        let processes = processes
            .into_iter()
            .filter(|process| process.uid == unsafe { libc::geteuid() })
            .collect::<Vec<_>>();
        let socket_pids = processes
            .iter()
            .filter(|process| {
                is_ssh_program(&process.args) || is_sshd_session_program(&process.args)
            })
            .map(|process| process.pid)
            .collect::<Vec<_>>();
        let tcp_connections = self.process_tcp_connections(&socket_pids);
        let parent_pids = processes
            .iter()
            .map(|process| (process.pid, process.parent_pid))
            .collect::<HashMap<_, _>>();
        let sshd_pids = processes
            .iter()
            .filter(|process| is_sshd_session_program(&process.args))
            .map(|process| process.pid)
            .collect::<HashSet<_>>();
        let sshd_connections = sshd_pids
            .iter()
            .filter_map(|pid| {
                ssh_transport_connection(tcp_connections.get(pid)?)
                    .map(|connection| (*pid, ssh_connection(&connection.right, &connection.left)))
            })
            .collect::<HashMap<_, _>>();
        let ssh_connections =
            unambiguous_ssh_connections(&processes, &parent_pids, &sshd_pids, &sshd_connections);
        let mosh_pids = processes
            .iter()
            .filter(|process| program_name(&process.args) == Some("mosh-server"))
            .map(|process| process.pid)
            .collect::<HashSet<_>>();
        let mosh_endpoints = if mosh_pids.is_empty() {
            HashMap::new()
        } else {
            let pids = mosh_pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            Command::new("lsof")
                .args(["-nP", "-a", "-p", &pids, "-iUDP", "-Fpn"])
                .output()
                .ok()
                .map(|output| parse_lsof_udp_endpoints(&output.stdout))
                .unwrap_or_default()
        };
        let client_connections = processes
            .iter()
            .filter_map(|process| {
                let connection =
                    if let Some(pid) = find_ancestor(process.pid, &parent_pids, &mosh_pids) {
                        ClientConnection::Mosh {
                            endpoint: mosh_endpoints.get(&pid)?.clone(),
                        }
                    } else {
                        ClientConnection::Ssh {
                            connection: ssh_connections.get(&process.pid)?.clone(),
                        }
                    };
                Some((process.pid, connection))
            })
            .collect();
        Ok(ProcessInventory {
            processes,
            process_started_at_ms,
            tcp_connections,
            parent_pids,
            ssh_connections,
            client_connections,
        })
    }

    fn project_process_snapshot(
        &self,
        panes: &[Pane],
        inventory: &ProcessInventory,
    ) -> ProcessSnapshot {
        let processes = inventory.processes.as_slice();
        let tcp_connections = &inventory.tcp_connections;
        let mut pane_descriptions = HashMap::new();
        let mut pane_groups = HashMap::new();
        let mut pane_pids = HashMap::new();
        let mut tmux_process_groups = HashSet::new();
        for pane in panes {
            if let Some((process_group, description, pids)) =
                foreground_job(pane.pane_pid, processes)
            {
                tmux_process_groups.insert(process_group);
                pane_groups.insert(pane.pane_id.clone(), process_group);
                pane_pids.insert(pane.pane_id.clone(), pids);
                pane_descriptions.insert(pane.pane_id.clone(), description);
            }
        }
        let mut ssh_transports = Vec::new();
        for pane in panes.iter().filter(|pane| !pane.dead && !pane.is_agent_ui) {
            let mut found_ssh = false;
            for process in pane_pids
                .get(&pane.pane_id)
                .into_iter()
                .flatten()
                .filter_map(|pid| processes.iter().find(|process| process.pid == *pid))
                .filter(|process| is_ssh_program(&process.args))
            {
                let Some(remote_host) = pane
                    .mirror_host
                    .as_deref()
                    .or_else(|| ssh_destination_for_command(&process.args))
                else {
                    continue;
                };
                found_ssh = true;
                let connection = tcp_connections
                    .get(&process.pid)
                    .and_then(|sockets| ssh_transport_connection(sockets))
                    .map(|connection| ssh_connection(&connection.left, &connection.right));
                ssh_transports.push(local_ssh_transport(
                    pane,
                    remote_host,
                    connection,
                    &self.host_aliases,
                ));
            }
            for process in pane_pids
                .get(&pane.pane_id)
                .into_iter()
                .flatten()
                .filter_map(|pid| processes.iter().find(|process| process.pid == *pid))
            {
                if let Some((host, endpoint)) = mosh_client_endpoint(&process.args) {
                    found_ssh = true;
                    let remote_host = pane.mirror_host.as_deref().unwrap_or(host);
                    let mut transport =
                        local_ssh_transport(pane, remote_host, None, &self.host_aliases);
                    transport.mosh_endpoint = Some(endpoint);
                    ssh_transports.push(transport);
                }
            }
            if !found_ssh && let Some(remote_host) = pane.mirror_host.as_deref() {
                ssh_transports.push(local_ssh_transport(
                    pane,
                    remote_host,
                    None,
                    &self.host_aliases,
                ));
            }
        }
        ssh_transports.sort_by(|left, right| {
            left.target
                .session_name
                .cmp(&right.target.session_name)
                .then_with(|| left.target.window_index.cmp(&right.target.window_index))
                .then_with(|| left.target.pane_index.cmp(&right.target.pane_index))
        });
        ssh_transports.dedup();
        ProcessSnapshot {
            panes: pane_descriptions,
            pane_groups,
            pane_pids,
            process_args: processes
                .iter()
                .map(|process| (process.pid, process.args.clone()))
                .collect(),
            process_started_at_ms: inventory.process_started_at_ms.clone(),
            live_pids: processes.iter().map(|process| process.pid).collect(),
            terminals: foreground_terminal_jobs(
                processes,
                &tmux_process_groups,
                &tmux_pane_terminals(panes, processes),
            ),
            parent_pids: inventory.parent_pids.clone(),
            ssh_connections: inventory.ssh_connections.clone(),
            client_connections: inventory.client_connections.clone(),
            ssh_transports,
        }
    }

    pub fn capture_visible_batch(&self, pane_ids: &[String]) -> HashMap<String, Result<String>> {
        if pane_ids.is_empty() {
            return HashMap::new();
        }
        let framing = CaptureBatchFraming::fresh();
        let command_args = capture_batch_args(pane_ids, &framing);
        let mut command = Command::new("tmux");
        command.args(&self.args).args(&command_args);
        match command.output() {
            Ok(output) => parse_capture_batch(
                pane_ids,
                &framing,
                &String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr).trim(),
            ),
            Err(error) => {
                let message = error.to_string();
                pane_ids
                    .iter()
                    .map(|pane_id| {
                        (
                            pane_id.clone(),
                            Err(anyhow::anyhow!(
                                "run tmux capture-pane batch for {pane_id}: {message}"
                            )),
                        )
                    })
                    .collect()
            }
        }
    }

    pub fn process_working_directories(&self, pids: &[u32]) -> HashMap<u32, String> {
        if pids.is_empty() {
            return HashMap::new();
        }
        let pid_list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        Command::new("lsof")
            .args(["-a", "-p", &pid_list, "-d", "cwd", "-Fn"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| parse_lsof_working_directories(&output.stdout))
            .unwrap_or_default()
    }

    fn process_tcp_connections(&self, pids: &[u32]) -> HashMap<u32, Vec<TcpSocket>> {
        if pids.is_empty() {
            return HashMap::new();
        }
        let pid_list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        Command::new("lsof")
            .args([
                "-nP",
                "-a",
                "-p",
                &pid_list,
                "-iTCP",
                "-sTCP:ESTABLISHED",
                "-Ffpn",
            ])
            .output()
            .ok()
            .map(|output| parse_lsof_tcp_connections(&output.stdout))
            .unwrap_or_default()
    }

    pub fn focus_agent(&self, record: &AgentRecord) -> Result<FocusOutcome> {
        if let Some(alias) = &record.remote_alias {
            if record.is_tmux()
                && let Some(connections) = &record.session_connections
            {
                let panes = self.list_panes()?;
                let processes = self.process_snapshot(&panes)?;
                let transports = session_transports(record, &processes.ssh_transports);
                let target = match transports.as_slice() {
                    [transport] => transport.target.clone(),
                    [] if !connections.complete => {
                        let pane = find_mirror_pane(&panes, alias, &record.session_name)?
                            .ok_or_else(|| FocusTargetMissing {
                                alias: alias.clone(),
                                title: record.title.clone(),
                                session: Some(record.session_name.clone()),
                                live_session: false,
                            })?;
                        local_ssh_transport(pane, alias, None, &self.host_aliases).target
                    }
                    [] => {
                        return Err(FocusTargetMissing {
                            alias: alias.clone(),
                            title: record.title.clone(),
                            session: Some(record.session_name.clone()),
                            live_session: true,
                        }
                        .into());
                    }
                    _ => bail!(
                        "multiple live local transports for {alias}/{}; detach duplicate clients, then refresh",
                        record.session_name
                    ),
                };
                self.focus_location(&target.session_name, &target.window_id, &target.pane_id)?;
                return Ok(FocusOutcome::TransportOnly);
            }
            if let Some(target) = &record.focus_target {
                self.focus_location(&target.session_name, &target.window_id, &target.pane_id)?;
                return Ok(if record.is_tmux() {
                    FocusOutcome::TransportOnly
                } else {
                    FocusOutcome::Exact
                });
            }
            if record.is_tmux() {
                if let Some((mirror, outcome)) = self.find_or_repair_mirror(alias, record)? {
                    self.focus_location(&mirror.session_name, &mirror.window_id, &mirror.pane_id)?;
                    return Ok(outcome);
                }
                if let Some(transport) = self.find_bound_host_transport(alias)? {
                    self.focus_location(
                        &transport.session_name,
                        &transport.window_id,
                        &transport.pane_id,
                    )?;
                    return Ok(FocusOutcome::TransportOnly);
                }
            } else if let Some(pane) = self.find_transport_pane(alias, &record.title)? {
                self.focus_location(&pane.session_name, &pane.window_id, &pane.pane_id)?;
                return Ok(FocusOutcome::Exact);
            }
            return Err(FocusTargetMissing {
                alias: alias.clone(),
                title: record.title.clone(),
                session: record.is_tmux().then(|| record.session_name.clone()),
                live_session: false,
            }
            .into());
        }
        if !record.is_tmux() {
            bail!(
                "{} is an ordinary terminal session and cannot be focused through tmux",
                record.location()
            );
        }
        self.focus_location(&record.session_name, &record.window_id, &record.pane_id)?;
        Ok(FocusOutcome::Exact)
    }

    fn find_or_repair_mirror(
        &self,
        remote_alias: &str,
        record: &AgentRecord,
    ) -> Result<Option<(Pane, FocusOutcome)>> {
        let panes = self.list_panes()?;
        if let Some(pane) = find_mirror_pane(&panes, remote_alias, &record.session_name)? {
            return Ok(Some((pane.clone(), FocusOutcome::TransportOnly)));
        }
        let Some(pane) =
            find_stale_mirror_pane(&panes, remote_alias, &record.session_name, &record.title)
        else {
            let processes = self.process_snapshot(&panes)?;
            if let Some(pane) =
                find_running_mosh_mirror(&panes, &processes.panes, remote_alias, &record.title)?
            {
                return Ok(Some((
                    self.mark_remote_mirror(pane, remote_alias, &record.session_name)?,
                    FocusOutcome::TransportOnly,
                )));
            }
            // Detached recovery selects the remote target and verifies attachment.
            // Finding or repairing an existing binding only locates its transport.
            return Ok(self
                .recover_detached_mirror(&panes, &processes.panes, remote_alias, record)?
                .map(|pane| (pane, FocusOutcome::Exact)));
        };
        self.status(&[
            "set-option",
            "-p",
            "-t",
            &pane.pane_id,
            REMOTE_SESSION_OPTION,
            &record.session_name,
        ])?;
        let mut repaired = pane.clone();
        repaired.mirror_session = Some(record.session_name.clone());
        Ok(Some((repaired, FocusOutcome::TransportOnly)))
    }

    fn recover_detached_mirror(
        &self,
        panes: &[Pane],
        pane_processes: &HashMap<String, String>,
        remote_alias: &str,
        record: &AgentRecord,
    ) -> Result<Option<Pane>> {
        if record.server != "default" {
            return Ok(None);
        }
        let Some(pane) =
            find_detached_mosh_shell(panes, pane_processes, remote_alias, &record.cwd)?
        else {
            return Ok(None);
        };

        let attach = shell_join(&[
            "tmux".into(),
            "attach-session".into(),
            "-t".into(),
            record.session_name.clone(),
        ]);
        let select_window = shell_join(&[
            "tmux".into(),
            "select-window".into(),
            "-t".into(),
            record.window_id.clone(),
        ]);
        let select_pane = shell_join(&[
            "tmux".into(),
            "select-pane".into(),
            "-t".into(),
            record.pane_id.clone(),
        ]);
        let sequence = format!("{select_window} && {select_pane} && {attach}");
        let command = shell_join(&["sh".into(), "-c".into(), sequence]);
        self.status(&["send-keys", "-l", "-t", &pane.pane_id, &command])?;
        self.status(&["send-keys", "-t", &pane.pane_id, "Enter"])?;

        let expected_title = normalize_transport_title(&record.title);
        let deadline = Instant::now() + REMOTE_ATTACH_TIMEOUT;
        let format = format!("#{{pane_dead}}{SEPARATOR}#{{pane_title}}");
        loop {
            let current =
                self.run_optional(&["display-message", "-p", "-t", &pane.pane_id, &format])?;
            let attached = current.as_deref().is_some_and(|value| {
                let value = value.trim_end();
                value
                    .split_once(SEPARATOR)
                    .or_else(|| value.split_once(ESCAPED_SEPARATOR))
                    .is_some_and(|(dead, title)| {
                        dead == "0" && normalize_transport_title(title) == expected_title
                    })
            });
            if attached {
                break;
            }
            if Instant::now() >= deadline {
                bail!(
                    "remote tmux session {}/{} did not attach in local pane {}",
                    terminal_safe(remote_alias),
                    terminal_safe(&record.session_name),
                    terminal_safe(&pane.pane_id)
                );
            }
            std::thread::sleep(REMOTE_ATTACH_POLL_INTERVAL);
        }

        Ok(Some(self.mark_remote_mirror(
            pane,
            remote_alias,
            &record.session_name,
        )?))
    }

    fn mark_remote_mirror(
        &self,
        pane: &Pane,
        remote_alias: &str,
        remote_session: &str,
    ) -> Result<Pane> {
        self.status(&[
            "set-option",
            "-p",
            "-t",
            &pane.pane_id,
            REMOTE_HOST_OPTION,
            remote_alias,
        ])?;
        self.status(&[
            "set-option",
            "-p",
            "-t",
            &pane.pane_id,
            REMOTE_SESSION_OPTION,
            remote_session,
        ])?;
        let mut recovered = pane.clone();
        recovered.mirror_host = Some(remote_alias.to_string());
        recovered.mirror_session = Some(remote_session.to_string());
        Ok(recovered)
    }

    fn find_transport_pane(&self, remote_alias: &str, title: &str) -> Result<Option<Pane>> {
        let panes = self.list_panes()?;
        let processes = self.process_snapshot(&panes)?;
        Ok(find_transport_pane(&panes, &processes.panes, remote_alias, title)?.cloned())
    }

    fn find_bound_host_transport(&self, remote_alias: &str) -> Result<Option<Pane>> {
        Ok(find_bound_host_transport(&self.list_panes()?, remote_alias)?.cloned())
    }

    pub fn set_ui_marker(&self, pane_id: &str, enabled: bool) -> Result<()> {
        let value = if enabled { "1" } else { "" };
        self.status(&["set-option", "-p", "-t", pane_id, "@tmux_agent_ui", value])
    }

    pub fn bind_remote_pane(
        &self,
        target: Option<&str>,
        remote: &str,
        session: &str,
    ) -> Result<String> {
        let pane = self.binding_pane(target, false)?;
        self.status(&[
            "set-option",
            "-p",
            "-t",
            &pane.pane_id,
            REMOTE_HOST_OPTION,
            remote,
        ])?;
        self.status(&[
            "set-option",
            "-p",
            "-t",
            &pane.pane_id,
            REMOTE_SESSION_OPTION,
            session,
        ])?;
        Ok(pane.pane_id)
    }

    pub fn unbind_remote_pane(&self, target: Option<&str>) -> Result<String> {
        let pane = self.binding_pane(target, true)?;
        self.status(&[
            "set-option",
            "-p",
            "-u",
            "-t",
            &pane.pane_id,
            REMOTE_HOST_OPTION,
        ])?;
        self.status(&[
            "set-option",
            "-p",
            "-u",
            "-t",
            &pane.pane_id,
            REMOTE_SESSION_OPTION,
        ])?;
        Ok(pane.pane_id)
    }

    pub fn remote_pane_bindings(&self) -> Result<Vec<RemotePaneBinding>> {
        Ok(self
            .list_panes()?
            .into_iter()
            .filter_map(|pane| {
                Some(RemotePaneBinding {
                    pane_id: pane.pane_id,
                    remote: pane.mirror_host?,
                    session: pane.mirror_session?,
                })
            })
            .collect())
    }

    fn binding_pane(&self, target: Option<&str>, allow_ui: bool) -> Result<Pane> {
        let pane_id = match target {
            Some(target) if !target.trim().is_empty() => target.to_string(),
            Some(_) => bail!("local pane ID cannot be empty"),
            None => env::var("TMUX_PANE")
                .ok()
                .filter(|pane_id| !pane_id.trim().is_empty())
                .context("no current local tmux pane; pass --pane <local-pane-id>")?,
        };
        let pane = self
            .list_panes()?
            .into_iter()
            .find(|pane| pane.pane_id == pane_id)
            .with_context(|| format!("no local tmux pane has ID {pane_id}"))?;
        if pane.dead {
            bail!("{pane_id} is dead");
        }
        if pane.is_agent_ui && !allow_ui {
            bail!("{pane_id} is a tmux-agent UI pane");
        }
        Ok(pane)
    }

    pub fn broadcast_ui_selection(&self, agent_id: &str) -> Result<()> {
        let pane_ids = selection_broadcast_panes(&self.list_panes()?);
        if pane_ids.is_empty() {
            return Ok(());
        }
        self.status(&["set-option", "-g", UI_SELECTION_OPTION, agent_id])?;
        wake_ui_panes(&pane_ids, |pane_id| {
            let mut args = vec!["send-keys", "-H", "-t", pane_id];
            args.extend(UI_SELECTION_WAKE_HEX);
            self.status(&args)
        })
    }

    pub fn ui_selection(&self) -> Result<Option<String>> {
        Ok(self
            .run_optional(&["show-option", "-gqv", UI_SELECTION_OPTION])?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()))
    }

    pub fn pane_visible(&self, pane_id: &str) -> Result<bool> {
        let format = format!("#{{window_active}}{SEPARATOR}#{{session_attached}}");
        let output = self.run(&["display-message", "-p", "-t", pane_id, &format])?;
        parse_pane_visibility(&output, pane_id)
    }

    pub fn display_popup(&self, command: &str) -> Result<()> {
        self.status(&["display-popup", "-E", "-w", "85%", "-h", "80%", command])
    }

    pub fn focus_location(&self, session: &str, window: &str, pane: &str) -> Result<()> {
        if has_current_tmux_client(std::env::var_os("TMUX").as_deref()) {
            self.status(&["switch-client", "-t", session])?;
        }
        self.status(&["select-window", "-t", window])?;
        self.status(&["select-pane", "-t", pane])?;
        Ok(())
    }

    fn run(&self, command_args: &[&str]) -> Result<String> {
        let mut command = Command::new("tmux");
        command.args(&self.args).args(command_args);
        let output = command
            .output()
            .with_context(|| format!("run tmux {}", command_args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("tmux {} failed: {}", command_args.join(" "), stderr);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn run_optional(&self, command_args: &[&str]) -> Result<Option<String>> {
        let mut command = Command::new("tmux");
        command.args(&self.args).args(command_args);
        let output = command
            .output()
            .with_context(|| format!("run tmux {}", command_args.join(" ")))?;
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if is_missing_server(&stderr) {
            return Ok(None);
        }
        bail!("tmux {} failed: {}", command_args.join(" "), stderr);
    }

    fn status(&self, command_args: &[&str]) -> Result<()> {
        self.run(command_args).map(|_| ())
    }
}

fn runtime_key_for(
    args: &[String],
    tmux_environment: Option<&str>,
    tmux_tmpdir: Option<&OsStr>,
    effective_uid: u32,
) -> String {
    if !args.is_empty() {
        return format!("tmux:{}", args.join("\u{1f}"));
    }
    let socket = tmux_environment
        .and_then(|value| value.split(',').next())
        .filter(|socket| !socket.is_empty());
    match socket {
        Some(socket)
            if normalized_socket_path(Path::new(socket))
                == normalized_socket_path(&default_socket_path(tmux_tmpdir, effective_uid)) =>
        {
            "default".to_string()
        }
        Some(socket) => format!("tmux-env:{socket}"),
        None => "default".to_string(),
    }
}

fn default_socket_path(tmux_tmpdir: Option<&OsStr>, effective_uid: u32) -> PathBuf {
    PathBuf::from(tmux_tmpdir.unwrap_or_else(|| OsStr::new("/tmp")))
        .join(format!("tmux-{effective_uid}"))
        .join("default")
}

fn configured_host_aliases(config: &Config) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    for machine in &config.machines {
        insert_host_alias(&mut aliases, &machine.name, &machine.name);
        insert_host_alias(&mut aliases, &machine.host, &machine.name);
    }
    for remote in &config.remotes {
        insert_host_alias(&mut aliases, &remote.name, &remote.name);
        let command = remote.command.join(" ");
        if let Some(host) = ssh_destination_for_command(&command) {
            insert_host_alias(&mut aliases, host, &remote.name);
        }
    }
    aliases
}

fn insert_host_alias(aliases: &mut HashMap<String, String>, host: &str, alias: &str) {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    aliases.insert(host.clone(), alias.to_string());
    if let Some((short, _)) = host.split_once('.') {
        aliases
            .entry(short.to_string())
            .or_insert_with(|| alias.to_string());
    }
}

fn resolved_host_alias(aliases: &HashMap<String, String>, host: &str) -> String {
    let key = host.trim_end_matches('.').to_ascii_lowercase();
    aliases
        .get(&key)
        .cloned()
        .unwrap_or_else(|| host.to_string())
}

fn local_ssh_transport(
    pane: &Pane,
    remote_host: &str,
    connection: Option<SshConnection>,
    aliases: &HashMap<String, String>,
) -> SshTransport {
    SshTransport {
        connection,
        mosh_endpoint: None,
        remote_host: resolved_host_alias(aliases, remote_host),
        remote_host_explicit: pane.mirror_host.is_some(),
        remote_session: pane.mirror_session.clone(),
        title: normalize_transport_title(&pane.title),
        label: pane.label.clone(),
        visible: pane.visible,
        target: TmuxTarget {
            session_name: pane.session_name.clone(),
            window_id: pane.window_id.clone(),
            window_index: pane.window_index,
            pane_id: pane.pane_id.clone(),
            pane_index: pane.pane_index,
        },
    }
}

fn normalized_socket_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

struct CaptureBatchFraming {
    nonce: String,
}

impl CaptureBatchFraming {
    fn fresh() -> Self {
        let sequence = CAPTURE_BATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        Self {
            nonce: format!("{:x}-{:x}-{sequence:x}", std::process::id(), timestamp),
        }
    }

    fn begin(&self, index: usize) -> String {
        format!("__TMUX_AGENT_CAPTURE_{}_{}_BEGIN__", self.nonce, index)
    }

    fn end(&self, index: usize) -> String {
        format!("__TMUX_AGENT_CAPTURE_{}_{}_END__", self.nonce, index)
    }
}

fn capture_batch_args(pane_ids: &[String], framing: &CaptureBatchFraming) -> Vec<String> {
    let mut args = Vec::with_capacity(pane_ids.len() * 16);
    for (index, pane_id) in pane_ids.iter().enumerate() {
        if !args.is_empty() {
            args.push(";".into());
        }
        args.extend([
            "display-message".into(),
            "-p".into(),
            framing.begin(index),
            ";".into(),
            "if-shell".into(),
            "-F".into(),
            "1".into(),
            format!("capture-pane -p -t {pane_id}"),
            ";".into(),
            "display-message".into(),
            "-p".into(),
            framing.end(index),
        ]);
    }
    args
}

fn parse_capture_batch(
    pane_ids: &[String],
    framing: &CaptureBatchFraming,
    output: &str,
    error: &str,
) -> HashMap<String, Result<String>> {
    let mut captures = HashMap::new();
    let mut remaining = output;
    for (index, pane_id) in pane_ids.iter().enumerate() {
        let begin = format!("{}\n", framing.begin(index));
        let end = format!("{}\n", framing.end(index));
        let captured = remaining.find(&begin).and_then(|begin_at| {
            let after_begin = &remaining[begin_at + begin.len()..];
            let end_at = after_begin.find(&end)?;
            let screen = &after_begin[..end_at];
            remaining = &after_begin[end_at + end.len()..];
            (!screen.is_empty()).then(|| screen.to_string())
        });
        let result = captured.ok_or_else(|| {
            let detail = if error.is_empty() {
                "capture produced no framed output"
            } else {
                error
            };
            anyhow::anyhow!("tmux capture-pane failed for {pane_id}: {detail}")
        });
        captures.insert(pane_id.clone(), result);
    }
    captures
}

fn parse_pane_visibility(line: &str, pane_id: &str) -> Result<bool> {
    let line = line.trim();
    let mut fields = line.split(SEPARATOR).collect::<Vec<_>>();
    if fields.len() != 2 {
        fields = line.split(ESCAPED_SEPARATOR).collect::<Vec<_>>();
    }
    if fields.len() != 2 {
        bail!(
            "unexpected tmux visibility record for {pane_id} with {} fields: {:?}",
            fields.len(),
            line
        );
    }
    Ok(pane_is_visible(fields[0], fields[1]))
}

fn parse_pane(line: &str) -> Result<Pane> {
    let mut fields = line.split(SEPARATOR).collect::<Vec<_>>();
    if fields.len() != 19 {
        fields = line.split(ESCAPED_SEPARATOR).collect::<Vec<_>>();
    }
    if fields.len() != 19 {
        bail!(
            "unexpected tmux pane record with {} fields: {:?}",
            fields.len(),
            line
        );
    }
    Ok(Pane {
        pane_id: fields[0].to_string(),
        pane_pid: parse_number(fields[1], "pane_pid")?,
        session_id: fields[2].to_string(),
        session_name: fields[3].to_string(),
        window_id: fields[4].to_string(),
        window_index: parse_number(fields[5], "window_index")?,
        window_name: fields[6].to_string(),
        pane_index: parse_number(fields[7], "pane_index")?,
        current_command: fields[8].to_string(),
        title: fields[9].to_string(),
        cwd: fields[10].to_string(),
        dead: fields[11] == "1",
        visible: pane_is_visible(fields[13], fields[14]),
        is_agent_ui: fields[15] == "1",
        mirror_host: nonempty(fields[16]),
        mirror_session: nonempty(fields[17]),
        label: nonempty(fields[18]),
    })
}

fn pane_is_visible(window_active: &str, session_attached: &str) -> bool {
    window_active == "1" && session_attached != "0"
}

#[cfg(target_os = "macos")]
fn parse_processes(output: &str, terminal_names: &MacosTerminalNames) -> Vec<Process> {
    parse_macos_processes_with(output, terminal_names, macos_device_name)
}

#[cfg(target_os = "macos")]
fn parse_macos_processes_with<F>(
    output: &str,
    terminal_names: &MacosTerminalNames,
    mut lookup: F,
) -> Vec<Process>
where
    F: FnMut(libc::dev_t) -> Option<String>,
{
    output
        .lines()
        .filter_map(|line| {
            parse_process_with_terminal_resolver(line, |terminal| {
                terminal_names.resolve_with(terminal, &mut lookup)
            })
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn parse_processes(output: &str) -> Vec<Process> {
    output
        .lines()
        .filter_map(|line| parse_process_with_terminal_resolver(line, named_terminal))
        .collect()
}

fn parse_process_with_terminal_resolver<F>(line: &str, resolve_terminal: F) -> Option<Process>
where
    F: FnOnce(&str) -> Option<String>,
{
    let mut fields = line.split_whitespace();
    let uid = fields.next()?.parse().ok()?;
    let pid = fields.next()?.parse().ok()?;
    let parent_pid = fields.next()?.parse().ok()?;
    let process_group = fields.next()?.parse().ok()?;
    let foreground_group = fields
        .next()?
        .parse::<i64>()
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0);
    let terminal = resolve_terminal(fields.next()?);
    fields.next()?;
    let args = fields.collect::<Vec<_>>().join(" ");
    Some(Process {
        uid,
        pid,
        parent_pid,
        process_group,
        foreground_group,
        terminal,
        args,
    })
}

#[cfg(any(not(target_os = "macos"), test))]
fn named_terminal(value: &str) -> Option<String> {
    (!matches!(value, "??" | "?")).then(|| value.to_string())
}

#[cfg(target_os = "macos")]
fn resolve_macos_terminal_with<F>(value: &str, lookup: F) -> Option<String>
where
    F: FnOnce(libc::dev_t) -> Option<String>,
{
    let (major, minor) = value.split_once('/')?;
    let major = major.parse::<i32>().ok()?;
    let minor = minor.parse::<i32>().ok()?;
    if major < 0 || minor < 0 {
        return None;
    }
    lookup(libc::makedev(major, minor))
}

#[cfg(target_os = "macos")]
fn macos_device_name(device: libc::dev_t) -> Option<String> {
    let _guard = DEVNAME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let name = unsafe { libc::devname(device, libc::S_IFCHR) };
    if name.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(name) }
        .to_str()
        .ok()
        .map(str::to_string)
}

fn parse_process_start(line: &str) -> Option<(u32, u64)> {
    let mut fields = line.split_whitespace();
    fields.next()?;
    let pid = fields.next()?.parse().ok()?;
    fields.next()?;
    fields.next()?;
    fields.next()?;
    fields.next()?;
    let elapsed_ms = parse_elapsed_ms(fields.next()?)?;
    Some((pid, now_ms().saturating_sub(elapsed_ms)))
}

fn parse_elapsed_ms(value: &str) -> Option<u64> {
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<u64>().ok()?, clock)
    } else {
        (0, value)
    };
    let parts = clock
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] => hours
            .checked_mul(3_600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    days.checked_mul(86_400)?
        .checked_add(seconds)?
        .checked_mul(1_000)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn foreground_job(shell_pid: u32, processes: &[Process]) -> Option<(u32, String, Vec<u32>)> {
    let foreground_group = processes
        .iter()
        .find(|process| process.pid == shell_pid)?
        .foreground_group?;
    let mut foreground = processes
        .iter()
        .filter(|process| process.process_group == foreground_group)
        .collect::<Vec<_>>();
    foreground.sort_by_key(|process| (process.pid != foreground_group, process.pid));
    let pids = foreground
        .iter()
        .map(|process| process.pid)
        .collect::<Vec<_>>();
    let description = foreground
        .into_iter()
        .map(|process| process.args.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Some((foreground_group, description, pids))
}

fn foreground_terminal_jobs(
    processes: &[Process],
    excluded_groups: &HashSet<u32>,
    excluded_terminals: &HashSet<String>,
) -> Vec<TerminalJob> {
    let mut groups: HashMap<(String, u32), Vec<&Process>> = HashMap::new();
    for process in processes {
        let Some(terminal) = &process.terminal else {
            continue;
        };
        if process.foreground_group != Some(process.process_group)
            || excluded_groups.contains(&process.process_group)
            || excluded_terminals.contains(terminal)
        {
            continue;
        }
        groups
            .entry((terminal.clone(), process.process_group))
            .or_default()
            .push(process);
    }

    let mut jobs = groups
        .into_iter()
        .map(|((name, process_group), mut processes)| {
            processes.sort_by_key(|process| (process.pid != process_group, process.pid));
            let leader_pid = processes
                .iter()
                .find(|process| process.pid == process_group)
                .or_else(|| processes.first())
                .map(|process| process.pid)
                .unwrap_or(process_group);
            let pids = processes
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>();
            let description = processes
                .into_iter()
                .map(|process| process.args.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            TerminalJob {
                name,
                process_group,
                leader_pid,
                pids,
                processes: description,
            }
        })
        .collect::<Vec<_>>();
    jobs.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.process_group.cmp(&right.process_group))
    });
    jobs
}

fn tmux_pane_terminals(panes: &[Pane], processes: &[Process]) -> HashSet<String> {
    let pane_pids = panes
        .iter()
        .map(|pane| pane.pane_pid)
        .collect::<HashSet<_>>();
    processes
        .iter()
        .filter(|process| pane_pids.contains(&process.pid))
        .filter_map(|process| process.terminal.clone())
        .collect()
}

fn find_ancestor(
    start_pid: u32,
    parents: &HashMap<u32, u32>,
    ancestors: &HashSet<u32>,
) -> Option<u32> {
    let mut pid = start_pid;
    for _ in 0..64 {
        if ancestors.contains(&pid) {
            return Some(pid);
        }
        let parent = parents.get(&pid).copied()?;
        if parent == 0 || parent == pid {
            return None;
        }
        pid = parent;
    }
    None
}

fn parse_lsof_working_directories(output: &[u8]) -> HashMap<u32, String> {
    let mut result = HashMap::new();
    let mut current_pid = None;
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(pid) = line.strip_prefix('p').and_then(|value| value.parse().ok()) {
            current_pid = Some(pid);
        } else if let (Some(pid), Some(path)) = (current_pid, line.strip_prefix('n')) {
            result.insert(pid, path.to_string());
        }
    }
    result
}

fn parse_lsof_tcp_connections(output: &[u8]) -> HashMap<u32, Vec<TcpSocket>> {
    let mut result = HashMap::<u32, Vec<TcpSocket>>::new();
    let mut current_pid = None;
    let mut current_fd = None;
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(pid) = line.strip_prefix('p').and_then(|value| value.parse().ok()) {
            current_pid = Some(pid);
            current_fd = None;
        } else if let Some(fd) = line.strip_prefix('f') {
            current_fd = fd
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok();
        } else if let (Some(pid), Some(fd), Some(name)) =
            (current_pid, current_fd, line.strip_prefix('n'))
            && let Some(connection) = parse_tcp_connection(name)
        {
            result
                .entry(pid)
                .or_default()
                .push(TcpSocket { fd, connection });
        }
    }
    for sockets in result.values_mut() {
        sockets.sort_by(|left, right| {
            left.fd
                .cmp(&right.fd)
                .then_with(|| sort_connection(&left.connection, &right.connection))
        });
        sockets.dedup();
    }
    result
}

fn parse_lsof_udp_endpoints(output: &[u8]) -> HashMap<u32, MoshEndpoint> {
    let mut candidates = HashMap::<u32, Vec<MoshEndpoint>>::new();
    let mut pid = None;
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse().ok();
        } else if let (Some(pid), Some(value)) = (pid, line.strip_prefix('n'))
            && let Some(endpoint) = parse_tcp_endpoint(value.split("->").next().unwrap_or(value))
        {
            let endpoint = MoshEndpoint {
                address: endpoint.address,
                port: endpoint.port,
            };
            let values = candidates.entry(pid).or_default();
            if !values.contains(&endpoint) {
                values.push(endpoint);
            }
        }
    }
    candidates
        .into_iter()
        .filter_map(|(pid, values)| match values.as_slice() {
            [endpoint] => Some((pid, endpoint.clone())),
            _ => None,
        })
        .collect()
}

fn mosh_client_endpoint(command: &str) -> Option<(&str, MoshEndpoint)> {
    let destination = mosh_destination_for_command(command);
    if !is_mosh_client(&command.split_whitespace().collect::<Vec<_>>()) {
        return None;
    }
    let (_, endpoint) = command.rsplit_once(" | ")?;
    let fields = endpoint.split_whitespace().collect::<Vec<_>>();
    let [address, port] = fields.as_slice() else {
        return None;
    };
    let address = address.parse::<std::net::IpAddr>().ok()?.to_string();
    let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
    Some((
        destination.unwrap_or(fields[0]),
        MoshEndpoint { address, port },
    ))
}

fn is_mosh_client(fields: &[&str]) -> bool {
    let Some(executable) = fields.first() else {
        return false;
    };
    if executable.rsplit('/').next() == Some("mosh-client") {
        return true;
    }
    let Some(separator) = fields.iter().position(|field| *field == "|") else {
        return false;
    };
    if fields.get(1) != Some(&"-#") || separator < 2 || fields.len() != separator + 3 {
        return false;
    }
    // Mosh preserves its original --client option in the -# process title.
    // Check its exact executable independently of host parsing: embedded SSH
    // arguments lose their quoting in this title. Never inspect a remote command.
    let options = &fields[2..separator];
    let mut declared_client = None;
    for (index, field) in options
        .iter()
        .take_while(|field| **field != "--")
        .enumerate()
    {
        if *field == "--client" {
            declared_client = options.get(index + 1).copied();
        } else if let Some(client) = field.strip_prefix("--client=") {
            declared_client = Some(client);
        }
    }
    declared_client == Some(*executable)
        && fields[separator + 1].parse::<std::net::IpAddr>().is_ok()
        && fields[separator + 2]
            .parse::<u16>()
            .is_ok_and(|port| port != 0)
}

fn same_address(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        value
            .parse::<std::net::IpAddr>()
            .ok()
            .map(|address| match address {
                std::net::IpAddr::V6(address) => address
                    .to_ipv4_mapped()
                    .map(std::net::IpAddr::V4)
                    .unwrap_or(std::net::IpAddr::V6(address)),
                address => address,
            })
    };
    normalize(left)
        .zip(normalize(right))
        .is_some_and(|(left, right)| left == right)
}

/// Resolve only current attachment evidence. Callers decide whether unavailable
/// inspection permits an explicit binding; authoritative absence never does.
pub fn session_transports<'a>(
    record: &AgentRecord,
    transports: &'a [SshTransport],
) -> Vec<&'a SshTransport> {
    let Some(session) = &record.session_connections else {
        return Vec::new();
    };
    let mut matches = transports
        .iter()
        .filter(|transport| {
            session.clients.iter().any(|client| match client {
                ClientConnection::Ssh { connection } => {
                    transport.connection.as_ref() == Some(connection)
                }
                ClientConnection::Mosh { endpoint } => {
                    transport.mosh_endpoint.as_ref().is_some_and(|local| {
                        endpoint.port == local.port
                            && (same_address(&endpoint.address, &local.address)
                                || (matches!(endpoint.address.as_str(), "*" | "0.0.0.0" | "::")
                                    && record.remote_alias.as_deref()
                                        == Some(transport.remote_host.as_str())))
                    })
                }
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.target.pane_id.cmp(&right.target.pane_id));
    matches.dedup_by(|left, right| left.target.pane_id == right.target.pane_id);
    matches
}

fn ssh_transport_connection(sockets: &[TcpSocket]) -> Option<TcpConnection> {
    sockets
        .iter()
        .min_by(|left, right| {
            left.fd
                .cmp(&right.fd)
                .then_with(|| sort_connection(&left.connection, &right.connection))
        })
        .map(|socket| socket.connection.clone())
}

fn unambiguous_ssh_connections(
    processes: &[Process],
    parents: &HashMap<u32, u32>,
    sshd_pids: &HashSet<u32>,
    sshd_connections: &HashMap<u32, SshConnection>,
) -> HashMap<u32, SshConnection> {
    let process_by_pid = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    let candidates = processes
        .iter()
        .filter_map(|process| {
            let sshd_pid = find_ancestor(process.pid, parents, sshd_pids)?;
            Some((
                process.pid,
                outer_terminal(process.pid, sshd_pid, &process_by_pid),
                sshd_connections.get(&sshd_pid)?.clone(),
            ))
        })
        .collect::<Vec<_>>();
    let mut terminals = HashMap::<SshConnection, HashSet<&str>>::new();
    for (_, terminal, connection) in &candidates {
        if let Some(terminal) = terminal {
            terminals
                .entry(connection.clone())
                .or_default()
                .insert(terminal);
        }
    }
    candidates
        .into_iter()
        .filter(|(_, _, connection)| {
            terminals
                .get(connection)
                .is_some_and(|terminals| terminals.len() == 1)
        })
        .map(|(pid, _, connection)| (pid, connection))
        .collect()
}

fn outer_terminal<'a>(
    start_pid: u32,
    ancestor_pid: u32,
    processes: &HashMap<u32, &'a Process>,
) -> Option<&'a str> {
    let mut pid = start_pid;
    let mut terminal = None;
    for _ in 0..64 {
        if pid == ancestor_pid {
            return terminal;
        }
        let process = processes.get(&pid)?;
        if let Some(current) = process.terminal.as_deref() {
            terminal = Some(current);
        }
        if process.parent_pid == 0 || process.parent_pid == pid {
            return None;
        }
        pid = process.parent_pid;
    }
    None
}

fn sort_connection(left: &TcpConnection, right: &TcpConnection) -> std::cmp::Ordering {
    left.left
        .address
        .cmp(&right.left.address)
        .then_with(|| left.left.port.cmp(&right.left.port))
        .then_with(|| left.right.address.cmp(&right.right.address))
        .then_with(|| left.right.port.cmp(&right.right.port))
}

fn parse_tcp_connection(value: &str) -> Option<TcpConnection> {
    let (left, right) = value.split_once("->")?;
    Some(TcpConnection {
        left: parse_tcp_endpoint(left)?,
        right: parse_tcp_endpoint(right)?,
    })
}

fn parse_tcp_endpoint(value: &str) -> Option<TcpEndpoint> {
    let (address, port) = if let Some(value) = value.strip_prefix('[') {
        let (address, port) = value.split_once("]:")?;
        (address, port)
    } else {
        value.rsplit_once(':')?
    };
    Some(TcpEndpoint {
        address: address.to_string(),
        port: port.parse().ok()?,
    })
}

fn ssh_connection(client: &TcpEndpoint, server: &TcpEndpoint) -> SshConnection {
    SshConnection {
        client_address: client.address.clone(),
        client_port: client.port,
        server_address: server.address.clone(),
        server_port: server.port,
    }
}

fn is_ssh_program(args: &str) -> bool {
    program_name(args).is_some_and(|program| program == "ssh")
}

fn is_sshd_session_program(args: &str) -> bool {
    program_name(args).is_some_and(|program| {
        let program = program.trim_end_matches(':');
        program == "sshd" || program == "sshd-session"
    })
}

fn program_name(args: &str) -> Option<&str> {
    args.split_whitespace()
        .next()
        .map(|program| program.rsplit('/').next().unwrap_or(program))
}

fn parse_number<T: std::str::FromStr>(value: &str, field: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("parse {field} value {value:?}: {error}"))
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn is_missing_server(stderr: &str) -> bool {
    stderr.contains("server exited unexpectedly")
        || stderr.contains("no server running")
        || stderr.contains("error connecting to")
        || stderr.contains("no sessions")
}

fn has_current_tmux_client(tmux_environment: Option<&std::ffi::OsStr>) -> bool {
    tmux_environment.is_some_and(|value| !value.is_empty())
}

fn find_mirror_pane<'a>(
    panes: &'a [Pane],
    remote_alias: &str,
    remote_session: &str,
) -> Result<Option<&'a Pane>> {
    let matches = panes
        .iter()
        .filter(|pane| !pane.dead && !pane.is_agent_ui)
        .filter(|pane| {
            pane.mirror_host.as_deref() == Some(remote_alias)
                && pane.mirror_session.as_deref() == Some(remote_session)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [pane] => Ok(Some(*pane)),
        _ => bail!(
            "multiple local panes map to {remote_alias}/{remote_session}: {}",
            matches
                .iter()
                .map(|pane| pane.pane_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn find_bound_host_transport<'a>(
    panes: &'a [Pane],
    remote_alias: &str,
) -> Result<Option<&'a Pane>> {
    let matches = panes
        .iter()
        .filter(|pane| !pane.dead && !pane.is_agent_ui)
        .filter(|pane| {
            pane.mirror_host.as_deref() == Some(remote_alias) && pane.mirror_session.is_some()
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [pane] => Ok(Some(*pane)),
        _ => bail!(
            "multiple local transport panes are bound to {}: {}",
            terminal_safe(remote_alias),
            matches
                .iter()
                .map(|pane| terminal_safe(&pane.pane_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn find_stale_mirror_pane<'a>(
    panes: &'a [Pane],
    remote_alias: &str,
    remote_session: &str,
    title: &str,
) -> Option<&'a Pane> {
    let title = normalize_bound_transport_title(title);
    if title.is_empty() {
        return None;
    }
    let mut matches = panes
        .iter()
        .filter(|pane| !pane.dead && !pane.is_agent_ui)
        .filter(|pane| pane.mirror_host.as_deref() == Some(remote_alias))
        .filter(|pane| {
            pane.mirror_session
                .as_deref()
                .is_some_and(|session| session != remote_session)
        })
        .filter(|pane| normalize_bound_transport_title(&pane.title) == title);
    let pane = matches.next()?;
    matches.next().is_none().then_some(pane)
}

fn find_running_mosh_mirror<'a>(
    panes: &'a [Pane],
    pane_processes: &HashMap<String, String>,
    remote_alias: &str,
    title: &str,
) -> Result<Option<&'a Pane>> {
    let title = normalize_transport_title(title);
    if title.is_empty() {
        return Ok(None);
    }
    let matches = panes
        .iter()
        .filter(|pane| !pane.dead && !pane.is_agent_ui)
        .filter(|pane| match (&pane.mirror_host, &pane.mirror_session) {
            (None, None) => true,
            (Some(marked_host), Some(_)) => marked_host != remote_alias,
            _ => false,
        })
        .filter(|pane| nested_mosh_title(&pane.title) == Some(title.as_str()))
        .filter(|pane| {
            pane_processes
                .get(&pane.pane_id)
                .map(String::as_str)
                .unwrap_or(&pane.current_command)
                .lines()
                .next()
                .and_then(mosh_destination_for_command)
                .is_some_and(|host| host == remote_alias)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [pane] => Ok(Some(*pane)),
        _ => bail!(
            "multiple running local mosh panes match {remote_alias} with nested tmux title {title:?}: {}",
            matches
                .iter()
                .map(|pane| terminal_safe(&pane.pane_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn nested_mosh_title(value: &str) -> Option<&str> {
    let title = value.trim().strip_prefix("[mosh]")?.trim_start();
    let title = title.strip_prefix('·')?.trim_start();
    let title = trim_braille_activity_prefix(title).trim_end();
    (!title.is_empty()).then_some(title)
}

fn find_detached_mosh_shell<'a>(
    panes: &'a [Pane],
    pane_processes: &HashMap<String, String>,
    remote_alias: &str,
    cwd: &str,
) -> Result<Option<&'a Pane>> {
    let matches = panes
        .iter()
        .filter(|pane| !pane.dead && !pane.is_agent_ui)
        .filter(|pane| pane.mirror_host.is_none() && pane.mirror_session.is_none())
        .filter(|pane| transport_shell_title_matches_cwd(&pane.title, cwd))
        .filter(|pane| {
            pane_processes
                .get(&pane.pane_id)
                .map(String::as_str)
                .unwrap_or(&pane.current_command)
                .lines()
                .next()
                .and_then(mosh_destination_for_command)
                .is_some_and(|host| host == remote_alias)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [pane] => Ok(Some(*pane)),
        _ => bail!(
            "multiple detached local mosh shells match {} at {}: {}",
            terminal_safe(remote_alias),
            terminal_safe(cwd),
            matches
                .iter()
                .map(|pane| terminal_safe(&pane.pane_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn transport_shell_title_matches_cwd(title: &str, cwd: &str) -> bool {
    let title = normalize_transport_title(title);
    let Some((_, displayed_cwd)) = title.split_once(':') else {
        return false;
    };
    let displayed_cwd = displayed_cwd.trim_end_matches('/');
    let cwd = cwd.trim_end_matches('/');
    if let Some(relative) = displayed_cwd.strip_prefix("~/") {
        return !relative.is_empty() && remote_home_relative(cwd) == Some(relative);
    }
    displayed_cwd.starts_with('/') && displayed_cwd == cwd
}

fn remote_home_relative(cwd: &str) -> Option<&str> {
    let mut components = Path::new(cwd).components();
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    match components.next()? {
        Component::Normal(directory) if directory == OsStr::new("root") => {}
        Component::Normal(directory)
            if directory == OsStr::new("Users") || directory == OsStr::new("home") =>
        {
            let Component::Normal(_) = components.next()? else {
                return None;
            };
        }
        _ => return None,
    }
    components.as_path().to_str()
}

fn find_transport_pane<'a>(
    panes: &'a [Pane],
    pane_processes: &HashMap<String, String>,
    remote_alias: &str,
    title: &str,
) -> Result<Option<&'a Pane>> {
    let title = normalize_transport_title(title);
    if title.is_empty() {
        return Ok(None);
    }
    let matches = panes
        .iter()
        .filter(|pane| !pane.dead && !pane.is_agent_ui)
        .filter(|pane| pane.mirror_host.is_none() && pane.mirror_session.is_none())
        .filter(|pane| normalize_transport_title(&pane.title) == title)
        .filter(|pane| {
            let processes = pane_processes
                .get(&pane.pane_id)
                .map(String::as_str)
                .unwrap_or(&pane.current_command);
            transport_destination(processes).is_some_and(|host| host == remote_alias)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [pane] => Ok(Some(*pane)),
        _ => bail!(
            "multiple local transport panes match {remote_alias} with title {title:?}: {}",
            matches
                .iter()
                .map(|pane| pane.pane_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn normalize_transport_title(value: &str) -> String {
    let title = value.trim();
    let title = title
        .strip_prefix("[mosh]")
        .map(str::trim_start)
        .unwrap_or(title);
    let title = title
        .strip_prefix('·')
        .map(str::trim_start)
        .unwrap_or(title);
    trim_braille_activity_prefix(title).trim_end().to_string()
}

fn normalize_bound_transport_title(value: &str) -> String {
    normalize_transport_title(value)
}

fn transport_destination(processes: &str) -> Option<&str> {
    processes
        .lines()
        .next()
        .and_then(transport_destination_for_command)
}

fn transport_destination_for_command(command: &str) -> Option<&str> {
    ssh_destination_for_command(command).or_else(|| mosh_destination_for_command(command))
}

fn mosh_destination_for_command(command: &str) -> Option<&str> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    if !is_mosh_client(&fields) {
        return None;
    }
    let separator = fields.iter().position(|field| *field == "|")?;
    let mut destination_index = 1;
    while destination_index < separator {
        let field = fields[destination_index];
        if field == "--" {
            destination_index += 1;
            break;
        }
        if !field.starts_with('-') {
            break;
        }
        let takes_value = matches!(
            field,
            "--client"
                | "--server"
                | "--predict"
                | "--port"
                | "-p"
                | "--family"
                | "--ssh"
                | "--bind-server"
                | "--experimental-remote-ip"
        );
        let flag = matches!(
            field,
            "-#" | "-a"
                | "-n"
                | "-o"
                | "-4"
                | "-6"
                | "--predict-overwrite"
                | "--no-predict-overwrite"
                | "--ssh-pty"
                | "--no-ssh-pty"
                | "--init"
                | "--no-init"
                | "--local"
                | "--help"
                | "--version"
                | "--fake-proxy"
                | "--no-fake-proxy"
        );
        let has_attached_value = (field.starts_with("-p=") && field.len() > 3)
            || [
                "--client=",
                "--server=",
                "--predict=",
                "--port=",
                "--family=",
                "--ssh=",
                "--bind-server=",
                "--experimental-remote-ip=",
            ]
            .iter()
            .any(|prefix| field.starts_with(prefix));
        destination_index += if takes_value {
            2
        } else if flag || has_attached_value {
            1
        } else {
            return None;
        };
    }
    if destination_index >= separator {
        return None;
    }
    let mut remote_command_separators = fields[destination_index + 1..separator]
        .iter()
        .enumerate()
        .filter_map(|(offset, field)| (*field == "--").then_some(destination_index + 1 + offset));
    let remote_command_separator = remote_command_separators.next();
    if remote_command_separators.next().is_some() {
        return None;
    }
    let alias = match remote_command_separator {
        Some(index) if index == destination_index + 1 && index + 1 < separator => {
            fields.get(destination_index)?
        }
        Some(_) => return None,
        None => separator
            .checked_sub(1)
            .and_then(|index| fields.get(index))?,
    };
    fields.get(separator + 1)?;
    fields.get(separator + 2)?;
    let alias = alias.rsplit('@').next().unwrap_or(alias);
    (!alias.is_empty() && !alias.starts_with('-')).then_some(alias)
}

fn ssh_destination_for_command(command: &str) -> Option<&str> {
    let mut fields = command.split_whitespace();
    let executable = fields.next()?;
    let program = executable.rsplit('/').next().unwrap_or(executable);
    if program != "ssh" {
        return None;
    }
    let mut options_ended = false;
    while let Some(field) = fields.next() {
        if options_ended {
            return Some(field.rsplit('@').next().unwrap_or(field));
        }
        if field == "--" {
            options_ended = true;
            continue;
        }
        if ssh_option_consumes_next(field) {
            fields.next()?;
            continue;
        }
        if field.starts_with('-') {
            continue;
        }
        return Some(field.rsplit('@').next().unwrap_or(field));
    }
    None
}

fn ssh_option_consumes_next(value: &str) -> bool {
    let Some(options) = value.strip_prefix('-') else {
        return false;
    };
    let mut options = options.chars().peekable();
    while let Some(option) = options.next() {
        if ssh_option_takes_operand(option) {
            return options.peek().is_none();
        }
    }
    false
}

fn ssh_option_takes_operand(value: char) -> bool {
    matches!(
        value,
        'B' | 'b'
            | 'c'
            | 'D'
            | 'E'
            | 'e'
            | 'F'
            | 'I'
            | 'i'
            | 'J'
            | 'L'
            | 'l'
            | 'm'
            | 'O'
            | 'o'
            | 'P'
            | 'p'
            | 'Q'
            | 'R'
            | 'S'
            | 'W'
            | 'w'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentOrigin, AgentState, Attention, EvidenceSource};
    use tempfile::tempdir;

    #[test]
    fn mosh_endpoint_parsing_handles_linux_and_macos_socket_records() {
        let sockets = parse_lsof_udp_endpoints(b"p10\nf4\nn[2001:db8::1]:60001\np20\nn192.0.2.1:60002\np30\nf4\nn*:60003\np40\nn127.0.0.1:60004\nn127.0.0.1:60005\n");
        assert_eq!(sockets[&10].address, "2001:db8::1");
        assert_eq!(sockets[&20].port, 60002);
        assert_eq!(sockets[&30].address, "*");
        assert!(!sockets.contains_key(&40));
        let (_, endpoint) = mosh_client_endpoint("/usr/bin/mosh-client -# --ssh=ssh -o BatchMode=yes remote-host -- tmux attach | 2001:db8::1 60001").unwrap();
        assert_eq!(endpoint, sockets[&10]);
        assert!(mosh_client_endpoint("mosh-client -# host | 127.0.0.1 0").is_none());
        assert!(mosh_client_endpoint("mosh-client -# host | <address> <port>").is_none());
        assert!(same_address("::ffff:192.0.2.1", "192.0.2.1"));
        assert!(same_address("2001:0db8:0::1", "2001:db8::1"));
    }

    #[test]
    fn custom_mosh_clients_require_the_exact_declared_executable() {
        for option in [
            "--client=/opt/bin/mobile-shell",
            "--client /opt/bin/mobile-shell",
        ] {
            let command = format!(
                "/opt/bin/mobile-shell -# {option} --no-init remote-host | 192.0.2.1 60001"
            );
            let (host, endpoint) = mosh_client_endpoint(&command).unwrap();
            assert_eq!(host, "remote-host");
            assert_eq!(endpoint.port, 60001);
            assert_eq!(mosh_destination_for_command(&command), Some("remote-host"));
        }
        let command = "/opt/bin/mobile-shell -# --client=/opt/bin/mobile-shell --ssh=ssh -o BatchMode=yes remote-host -- tmux attach | 192.0.2.1 60001";
        assert!(mosh_destination_for_command(command).is_none());
        assert_eq!(
            mosh_client_endpoint(command).unwrap().1.address,
            "192.0.2.1"
        );
        for command in [
            "/opt/bin/mobile-shell -# remote-host | 192.0.2.1 60001",
            "/opt/bin/mobile-shell --client=/opt/bin/mobile-shell remote-host | 192.0.2.1 60001",
            "/opt/bin/mobile-shell -# --client=/other/mobile-shell remote-host | 192.0.2.1 60001",
            "/opt/bin/mobile-shell -# remote-host -- program --client=/opt/bin/mobile-shell | 192.0.2.1 60001",
            "/opt/bin/mobile-shell -# --client=/opt/bin/mobile-shell remote-host | invalid 60001",
            "/opt/bin/mobile-shell -# --client=/opt/bin/mobile-shell remote-host | 192.0.2.1 0",
            "sleep 30 | 192.0.2.1 60001",
            "/opt/bin/mobile-shell -# --client=/opt/bin/mobile-shell --client=/other/client remote-host | 192.0.2.1 60001",
        ] {
            assert!(
                mosh_client_endpoint(command).is_none(),
                "accepted {command}"
            );
            assert!(
                mosh_destination_for_command(command).is_none(),
                "accepted {command}"
            );
        }
    }

    #[test]
    fn live_mosh_attachments_focus_hidden_windows_and_follow_session_switches() {
        const SOCKET_ENV: &str = "TMUX_AGENT_LIVE_ATTACHMENT_TEST";
        if let Ok(local_socket) = env::var(SOCKET_ENV) {
            let remote_socket = format!("{local_socket}-remote");
            struct Servers(Vec<String>);
            impl Drop for Servers {
                fn drop(&mut self) {
                    for socket in &self.0 {
                        let _ = Command::new("tmux")
                            .args(["-L", socket, "kill-server"])
                            .output();
                    }
                }
            }
            let _servers = Servers(vec![remote_socket.clone(), local_socket.clone()]);
            let custom_client_directory = tempdir().unwrap();
            let custom_client = custom_client_directory.path().join("mobile-shell");
            let installed_client = Command::new("sh")
                .args(["-c", "command -v mosh-client"])
                .output()
                .unwrap();
            assert!(installed_client.status.success());
            std::os::unix::fs::symlink(
                String::from_utf8(installed_client.stdout).unwrap().trim(),
                &custom_client,
            )
            .unwrap();
            let local = Tmux::new(&Config {
                tmux_args: vec!["-L".into(), local_socket.clone()],
                ..Config::default()
            });
            let remote = Tmux::new(&Config {
                tmux_args: vec!["-L".into(), remote_socket.clone()],
                ..Config::default()
            });
            let ui = new_test_tmux_pane(&local_socket, "local", None);
            test_tmux_output(
                &local_socket,
                &["set-option", "-pt", &ui, "@tmux_agent_ui", "1"],
            );
            for session in ["s", "t"] {
                new_test_tmux_command_pane(&remote_socket, session, None, "sh");
                test_tmux_output(
                    &remote_socket,
                    &[
                        "new-window",
                        "-d",
                        "-t",
                        &format!("{session}:"),
                        "-n",
                        "hidden-agent",
                        "sleep 60",
                    ],
                );
            }
            let attach = |session: &str| {
                let mut args = vec![
                    "python3".into(),
                    format!(
                        "{}/tests/fixtures/mosh-attachment.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    remote_socket.clone(),
                    session.into(),
                ];
                if session == "t" {
                    args.push(custom_client.to_string_lossy().into_owned());
                    args.push("0.0.0.0".into());
                }
                let command = shell_join(&args);
                test_tmux_value(
                    &local_socket,
                    &[
                        "new-window",
                        "-d",
                        "-t",
                        "local:",
                        "-P",
                        "-F",
                        "#{pane_id}",
                        &command,
                    ],
                )
            };
            let first = attach("s");
            let second = attach("t");
            mark_test_remote_pane(&local_socket, &second, "remote-mac", "t", "active shell");
            test_tmux_output(
                &local_socket,
                &[
                    "set-option",
                    "-pt",
                    &second,
                    "@pane_label",
                    "explicit transport",
                ],
            );
            let record = |session: &str| {
                let panes = remote.list_panes().unwrap();
                let pane = panes
                    .iter()
                    .find(|pane| pane.session_name == session && pane.window_name == "hidden-agent")
                    .unwrap();
                let mut record =
                    remote_tmux_record(session, "hidden agent unrelated to shell title");
                record.session_id = pane.session_id.clone();
                record.pane_id = pane.pane_id.clone();
                record.window_id = pane.window_id.clone();
                record.visible = false;
                let processes = remote.process_snapshot(&panes).unwrap();
                record.session_connections = remote
                    .session_connections(&processes)
                    .unwrap()
                    .remove(&record.session_id);
                record
            };
            let wait_for = |condition: &dyn Fn() -> bool| {
                let deadline = Instant::now() + Duration::from_secs(8);
                while !condition() {
                    assert!(
                        Instant::now() < deadline,
                        "live attachment did not reach expected state: s={:?}, t={:?}, local panes={:?}",
                        record("s").session_connections,
                        record("t").session_connections,
                        local
                            .list_panes()
                            .unwrap()
                            .iter()
                            .map(|pane| (&pane.pane_id, &pane.current_command))
                            .collect::<Vec<_>>()
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
            };
            wait_for(&|| {
                ["s", "t"].iter().all(|session| {
                    record(session)
                        .session_connections
                        .as_ref()
                        .is_some_and(|connections| {
                            connections.complete && connections.clients.len() == 1
                        })
                })
            });
            for (session, expected) in [("s", &first), ("t", &second)] {
                wait_for(&|| local.focus_agent(&record(session)).is_ok());
                assert_eq!(
                    local.focus_agent(&record(session)).unwrap(),
                    FocusOutcome::TransportOnly
                );
                assert_eq!(
                    test_tmux_value(&local_socket, &["display-message", "-p", "#{pane_id}"]),
                    *expected
                );
                assert_eq!(
                    test_tmux_value(
                        &remote_socket,
                        &["display-message", "-p", "-t", session, "#{window_index}"]
                    ),
                    "0"
                );
            }
            // The wildcard-bound remote server needs host attribution. Explicit
            // markers survive a title whose embedded SSH options hide its host.
            let processes = local
                .process_snapshot(&local.list_panes().unwrap())
                .unwrap();
            let transport = processes
                .ssh_transports
                .iter()
                .find(|transport| transport.target.pane_id == second)
                .unwrap();
            assert_eq!(transport.remote_host, "remote-mac");
            assert!(transport.remote_host_explicit);
            assert_eq!(
                transport.mosh_endpoint.as_ref().unwrap().address,
                "127.0.0.1"
            );
            let mut records = vec![record("t")];
            assert!(
                matches!(&records[0].session_connections.as_ref().unwrap().clients[0], ClientConnection::Mosh { endpoint } if matches!(endpoint.address.as_str(), "0.0.0.0" | "*"))
            );
            crate::daemon::reconcile_transports(&mut records, &processes.ssh_transports);
            assert_eq!(records[0].focus_target.as_ref().unwrap().pane_id, second);
            assert_eq!(records[0].label.as_deref(), Some("explicit transport"));
            assert!(!records[0].visible);
            // An obsolete explicit binding cannot override a supported live association.
            mark_test_remote_pane(&local_socket, &first, "remote-mac", "s", "unrelated shell");
            let first_client = test_tmux_value(
                &remote_socket,
                &["list-clients", "-t", "s", "-F", "#{client_name}"],
            );
            test_tmux_output(
                &remote_socket,
                &["switch-client", "-c", &first_client, "-t", "t"],
            );
            let missing = local.focus_agent(&record("s")).unwrap_err();
            assert!(missing.to_string().contains("no live local transport"));
            assert!(is_focus_target_missing(&missing));
            assert!(
                local
                    .focus_agent(&record("t"))
                    .unwrap_err()
                    .to_string()
                    .contains("multiple live local transports")
            );
            test_tmux_output(
                &local_socket,
                &["set-option", "-pt", &first, "@tmux_agent_ui", "1"],
            );
            assert_eq!(
                local.focus_agent(&record("t")).unwrap(),
                FocusOutcome::TransportOnly
            );
            assert_eq!(
                test_tmux_value(&local_socket, &["display-message", "-p", "#{pane_id}"]),
                second
            );
            test_tmux_output(
                &local_socket,
                &["set-option", "-pt", &first, "@tmux_agent_ui", "0"],
            );
            test_tmux_output(&remote_socket, &["detach-client", "-t", &first_client]);
            wait_for(&|| {
                record("t")
                    .session_connections
                    .as_ref()
                    .unwrap()
                    .clients
                    .len()
                    == 1
            });
            assert_eq!(
                local.focus_agent(&record("t")).unwrap(),
                FocusOutcome::TransportOnly
            );
            let reconnected = attach("s");
            wait_for(&|| local.focus_agent(&record("s")).is_ok());
            assert_eq!(
                test_tmux_value(&local_socket, &["display-message", "-p", "#{pane_id}"]),
                reconnected
            );
            test_tmux_output(
                &local_socket,
                &["set-option", "-wt", &reconnected, "remain-on-exit", "on"],
            );
            let client = test_tmux_value(
                &remote_socket,
                &["list-clients", "-t", "s", "-F", "#{client_name}"],
            );
            test_tmux_output(&remote_socket, &["detach-client", "-t", &client]);
            assert!(local.focus_agent(&record("s")).is_err());
            return;
        }
        for program in ["tmux", "mosh-client", "mosh-server", "python3", "lsof"] {
            if Command::new(program).arg("--version").output().is_err() {
                eprintln!("skipping live Mosh test: {program} unavailable");
                return;
            }
        }
        let directory = tempdir().unwrap();
        let status = Command::new(env::current_exe().unwrap())
            .args(["--exact", "tmux::tests::live_mosh_attachments_focus_hidden_windows_and_follow_session_switches", "--nocapture"])
            .env(SOCKET_ENV, format!("tmux-agent-live-{}", std::process::id()))
            .env("TMUX_TMPDIR", directory.path())
            .env_remove("TMUX").env_remove("TMUX_PANE").status().unwrap();
        assert!(status.success());
    }

    fn pane(pane_id: &str, session: &str, title: &str) -> Pane {
        Pane {
            pane_id: pane_id.into(),
            pane_pid: 100,
            session_id: format!("${session}"),
            session_name: session.into(),
            window_id: "@1".into(),
            window_index: 1,
            window_name: "work".into(),
            pane_index: 0,
            current_command: "ssh".into(),
            title: title.into(),
            label: None,
            cwd: "/work".into(),
            visible: true,
            dead: false,
            is_agent_ui: false,
            mirror_host: None,
            mirror_session: None,
        }
    }

    fn remote_tmux_record(session: &str, title: &str) -> AgentRecord {
        AgentRecord {
            id: format!("remote/remote-mac/host/default/{session}"),
            host: "host".into(),
            server: "default".into(),
            pane_id: "%0".into(),
            pane_pid: 100,
            session_id: "$0".into(),
            session_name: session.into(),
            window_id: "@0".into(),
            window_index: 0,
            window_name: "work".into(),
            pane_index: 0,
            agent: "Codex".into(),
            state: AgentState::Idle,
            attention: Attention::Idle,
            source: EvidenceSource::Screen,
            title: title.into(),
            label: None,
            cwd: "/work".into(),
            visible: true,
            seen: true,
            changed_at_ms: 1,
            origin: AgentOrigin::Tmux,
            terminal: None,
            remote_alias: Some("remote-mac".into()),
            ssh_connection: None,
            session_connections: None,
            focus_target: None,
            goal: None,
            subagent: None,
            detection: None,
        }
    }

    fn remote_terminal_record(title: &str) -> AgentRecord {
        let mut record = remote_tmux_record("terminal", title);
        record.id = "remote/remote-mac/terminal/synthetic-tty".into();
        record.origin = AgentOrigin::Terminal;
        record.terminal = Some("synthetic-tty".into());
        record
    }

    fn test_tmux_output(socket_name: &str, args: &[&str]) -> std::process::Output {
        let output = Command::new("tmux")
            .arg("-L")
            .arg(socket_name)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn test_tmux_value(socket_name: &str, args: &[&str]) -> String {
        String::from_utf8(test_tmux_output(socket_name, args).stdout)
            .unwrap()
            .trim()
            .to_string()
    }

    fn new_test_tmux_pane(socket_name: &str, session: &str, target: Option<&str>) -> String {
        new_test_tmux_command_pane(socket_name, session, target, "sleep 30")
    }

    fn new_test_tmux_command_pane(
        socket_name: &str,
        session: &str,
        target: Option<&str>,
        command: &str,
    ) -> String {
        match target {
            None => test_tmux_value(
                socket_name,
                &[
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    session,
                    "-x",
                    "200",
                    "-y",
                    "80",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    command,
                ],
            ),
            Some(target) => test_tmux_value(
                socket_name,
                &[
                    "split-window",
                    "-d",
                    "-t",
                    target,
                    "-P",
                    "-F",
                    "#{pane_id}",
                    command,
                ],
            ),
        }
    }

    fn wait_for_test_process_title(socket_name: &str, pane_id: &str, expected: &str) {
        let pane_pid = test_tmux_value(
            socket_name,
            &["display-message", "-p", "-t", pane_id, "#{pane_pid}"],
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let output = Command::new("ps")
                .args(["-p", &pane_pid, "-o", "args="])
                .output()
                .unwrap();
            let args = String::from_utf8_lossy(&output.stdout);
            if args.contains(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "pane {pane_id} never adopted the expected process title"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_test_shell_probe(socket_name: &str, pane_id: &str, probe_path: &Path) {
        let command =
            crate::config::shell_join(&["touch".into(), probe_path.to_string_lossy().into_owned()]);
        test_tmux_output(socket_name, &["send-keys", "-l", "-t", pane_id, &command]);
        test_tmux_output(socket_name, &["send-keys", "-t", pane_id, "Enter"]);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !probe_path.is_file() {
            assert!(
                Instant::now() < deadline,
                "pane {pane_id} did not consume its shell readiness probe"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_test_file_contents(path: &Path, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(path).ok().as_deref() == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} did not contain the expected test output",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn set_test_pane_title(socket_name: &str, pane_id: &str, title: &str) {
        test_tmux_output(socket_name, &["select-pane", "-t", pane_id, "-T", title]);
    }

    fn mark_test_remote_pane(
        socket_name: &str,
        pane_id: &str,
        host: &str,
        session: &str,
        title: &str,
    ) {
        test_tmux_output(socket_name, &["select-pane", "-t", pane_id, "-T", title]);
        test_tmux_output(
            socket_name,
            &["set-option", "-p", "-t", pane_id, REMOTE_HOST_OPTION, host],
        );
        test_tmux_output(
            socket_name,
            &[
                "set-option",
                "-p",
                "-t",
                pane_id,
                REMOTE_SESSION_OPTION,
                session,
            ],
        );
    }

    fn test_pane_option(socket_name: &str, pane_id: &str, option: &str) -> String {
        test_tmux_value(socket_name, &["show-option", "-pqv", "-t", pane_id, option])
    }

    #[test]
    fn focus_distinguishes_remote_transport_from_exact_targets() {
        const SOCKET_ENV: &str = "TMUX_AGENT_TRANSPORT_ONLY_FOCUS_TEST_SOCKET";
        if let Some(socket_name) = std::env::var_os(SOCKET_ENV) {
            let socket_name = socket_name.to_string_lossy().into_owned();
            let current = new_test_tmux_pane(&socket_name, "local", None);
            let transport = test_tmux_value(
                &socket_name,
                &[
                    "new-window",
                    "-d",
                    "-t",
                    "local",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "sleep 30",
                ],
            );
            mark_test_remote_pane(
                &socket_name,
                &transport,
                "remote-mac",
                "other-session",
                "[mosh] · other-project",
            );
            test_tmux_output(&socket_name, &["select-window", "-t", "local:0"]);
            test_tmux_output(&socket_name, &["select-pane", "-t", &current]);

            let config = Config {
                tmux_args: vec!["-L".into(), socket_name.clone()],
                ..Config::default()
            };
            let tmux = Tmux::new(&config);

            let outcome = tmux
                .focus_agent(&remote_tmux_record("selected-session", "selected-project"))
                .unwrap();

            assert_eq!(outcome, FocusOutcome::TransportOnly);
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &transport,
                        "#{window_active}:#{pane_active}"
                    ],
                ),
                "1:1"
            );
            assert_eq!(
                test_pane_option(&socket_name, &transport, REMOTE_HOST_OPTION),
                "remote-mac"
            );
            assert_eq!(
                test_pane_option(&socket_name, &transport, REMOTE_SESSION_OPTION),
                "other-session"
            );

            // A matching session binding does not prove its background target.
            new_test_tmux_pane(&socket_name, "other-session", None);
            let background = test_tmux_value(
                &socket_name,
                &[
                    "new-window",
                    "-d",
                    "-t",
                    "other-session",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "sleep 30",
                ],
            );
            let mut remote = remote_tmux_record("other-session", "background-project");
            remote.window_id = test_tmux_value(
                &socket_name,
                &["display-message", "-p", "-t", &background, "#{window_id}"],
            );
            remote.pane_id = background.clone();
            test_tmux_output(&socket_name, &["select-window", "-t", "local:0"]);
            assert_eq!(
                tmux.focus_agent(&remote).unwrap(),
                FocusOutcome::TransportOnly
            );
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &transport,
                        "#{window_active}:#{pane_active}"
                    ]
                ),
                "1:1"
            );
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &background,
                        "#{window_active}"
                    ]
                ),
                "0",
                "the inner background window must remain unchanged"
            );

            remote.window_id = "@99999".into();
            remote.pane_id = "%99999".into();
            assert_eq!(
                tmux.focus_agent(&remote).unwrap(),
                FocusOutcome::TransportOnly
            );

            let window_id = test_tmux_value(
                &socket_name,
                &["display-message", "-p", "-t", &transport, "#{window_id}"],
            );
            remote.focus_target = Some(crate::model::TmuxTarget {
                session_name: "local".into(),
                window_id: window_id.clone(),
                window_index: 1,
                pane_id: transport.clone(),
                pane_index: 0,
            });
            test_tmux_output(&socket_name, &["select-window", "-t", "local:0"]);
            assert_eq!(
                tmux.focus_agent(&remote).unwrap(),
                FocusOutcome::TransportOnly
            );
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &transport,
                        "#{window_active}:#{pane_active}"
                    ]
                ),
                "1:1"
            );

            let mut terminal = remote_terminal_record("ordinary-terminal");
            terminal.focus_target = remote.focus_target.clone();
            assert_eq!(tmux.focus_agent(&terminal).unwrap(), FocusOutcome::Exact);

            let mut local = remote_tmux_record("local", "local-project");
            local.remote_alias = None;
            local.window_id = window_id;
            local.pane_id = transport.clone();
            test_tmux_output(&socket_name, &["select-window", "-t", "local:0"]);
            assert_eq!(tmux.focus_agent(&local).unwrap(), FocusOutcome::Exact);
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &transport,
                        "#{window_active}:#{pane_active}"
                    ]
                ),
                "1:1"
            );
            local.origin = AgentOrigin::Terminal;
            assert!(
                tmux.focus_agent(&local)
                    .unwrap_err()
                    .to_string()
                    .contains("ordinary terminal")
            );

            let second = new_test_tmux_pane(&socket_name, "local", Some(&transport));
            mark_test_remote_pane(
                &socket_name,
                &second,
                "remote-mac",
                "second-session",
                "[mosh] · second-project",
            );
            let error = tmux
                .focus_agent(&remote_tmux_record("missing-session", "missing-project"))
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("multiple local transport panes are bound to remote-mac")
            );
            assert!(error.to_string().contains(&transport));
            assert!(error.to_string().contains(&second));
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!(
            "tmux-agent-transport-only-focus-{}-{nonce}",
            std::process::id()
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux::tests::focus_distinguishes_remote_transport_from_exact_targets",
                "--nocapture",
            ])
            .env(SOCKET_ENV, &socket_name)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn remote_tmux_focus_repairs_only_a_unique_stale_mosh_binding() {
        const SOCKET_ENV: &str = "TMUX_AGENT_REMOTE_FOCUS_TEST_SOCKET";
        if let Some(socket_name) = std::env::var_os(SOCKET_ENV) {
            let socket_name = socket_name.to_string_lossy().into_owned();
            let pane_id = new_test_tmux_pane(&socket_name, "local", None);
            mark_test_remote_pane(
                &socket_name,
                &pane_id,
                "remote-mac",
                "old-session",
                "[mosh] · recovered-project",
            );

            let config = Config {
                tmux_args: vec!["-L".into(), socket_name.clone()],
                ..Config::default()
            };
            let tmux = Tmux::new(&config);
            assert_eq!(
                tmux.focus_agent(&remote_tmux_record("0", "recovered-project"))
                    .unwrap(),
                FocusOutcome::TransportOnly
            );
            assert_eq!(
                test_pane_option(&socket_name, &pane_id, REMOTE_SESSION_OPTION),
                "0"
            );
            assert_eq!(
                test_pane_option(&socket_name, &pane_id, REMOTE_HOST_OPTION),
                "remote-mac"
            );

            let stale = new_test_tmux_pane(&socket_name, "exact", None);
            let exact = new_test_tmux_pane(&socket_name, "exact", Some(&stale));
            mark_test_remote_pane(
                &socket_name,
                &stale,
                "remote-mac",
                "old-exact",
                "[mosh] · exact-title",
            );
            mark_test_remote_pane(
                &socket_name,
                &exact,
                "remote-mac",
                "fresh-exact",
                "unrelated-title",
            );
            test_tmux_output(&socket_name, &["select-pane", "-t", &stale]);
            assert_eq!(
                tmux.focus_agent(&remote_tmux_record("fresh-exact", "exact-title"))
                    .unwrap(),
                FocusOutcome::TransportOnly
            );
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &["display-message", "-p", "-t", &exact, "#{pane_active}"],
                ),
                "1"
            );
            assert_eq!(
                test_pane_option(&socket_name, &stale, REMOTE_SESSION_OPTION),
                "old-exact"
            );

            let first = new_test_tmux_pane(&socket_name, "ambiguous", None);
            let second = new_test_tmux_pane(&socket_name, "ambiguous", Some(&first));
            mark_test_remote_pane(
                &socket_name,
                &first,
                "remote-mac",
                "old-first",
                "[mosh] · ambiguous-title",
            );
            mark_test_remote_pane(
                &socket_name,
                &second,
                "remote-mac",
                "old-second",
                "[mosh] · ambiguous-title",
            );
            let error = tmux
                .focus_agent(&remote_tmux_record("fresh-ambiguous", "ambiguous-title"))
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("multiple local transport panes are bound to remote-mac")
            );
            assert_eq!(
                test_pane_option(&socket_name, &first, REMOTE_SESSION_OPTION),
                "old-first"
            );
            assert_eq!(
                test_pane_option(&socket_name, &second, REMOTE_SESSION_OPTION),
                "old-second"
            );
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!("tmux-agent-remote-focus-{}-{nonce}", std::process::id());
        // A child without the inherited TMUX value can focus the isolated
        // server without trying to switch a client on the developer's server.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux::tests::remote_tmux_focus_repairs_only_a_unique_stale_mosh_binding",
                "--nocapture",
            ])
            .env(SOCKET_ENV, &socket_name)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn remote_tmux_focus_recovers_a_detached_session_through_one_idle_mosh_shell() {
        const SOCKET_ENV: &str = "TMUX_AGENT_REMOTE_ATTACH_TEST_SOCKET";
        if let Some(socket_name) = std::env::var_os(SOCKET_ENV) {
            use std::os::unix::fs::PermissionsExt;

            let socket_name = socket_name.to_string_lossy().into_owned();
            let fixture = tempdir().unwrap();
            let log_path = fixture.path().join("attach.log");
            let tmux_shim = fixture.path().join("tmux");
            std::fs::write(
                &tmux_shim,
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$TMUX_AGENT_ATTACH_LOG"
if [ "$1" = select-window ] || [ "$1" = select-pane ]; then
    exit 0
fi
if [ "$1" = attach-session ]; then
    if [ "$3" = recovered-session ]; then
        "$TMUX_AGENT_REAL_TMUX" -L "$TMUX_AGENT_TEST_SOCKET" \
            select-pane -t "$TMUX_PANE" -T '[mosh] · recovered-project'
    fi
    exit 0
fi
exit 1
"#,
            )
            .unwrap();
            let mut permissions = std::fs::metadata(&tmux_shim).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&tmux_shim, permissions).unwrap();

            let unrelated = new_test_tmux_pane(&socket_name, "local", None);
            mark_test_remote_pane(
                &socket_name,
                &unrelated,
                "remote-mac",
                "other-session",
                "[mosh] · other-project",
            );
            let path = format!(
                "{}:{}",
                fixture.path().display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let shell = crate::config::shell_join(&[
                "env".into(),
                "-u".into(),
                "TMUX".into(),
                format!("PATH={path}"),
                format!("TMUX_AGENT_ATTACH_LOG={}", log_path.display()),
                format!(
                    "TMUX_AGENT_REAL_TMUX={}",
                    std::env::var("TMUX_AGENT_TEST_TMUX").unwrap()
                ),
                format!("TMUX_AGENT_TEST_SOCKET={socket_name}"),
                "/bin/bash".into(),
                "-c".into(),
                "exec -a 'mosh-client -# --no-init remote-mac | <address> <port>' /bin/bash --noprofile --norc -i".into(),
            ]);
            let recoverable =
                new_test_tmux_command_pane(&socket_name, "local", Some(&unrelated), &shell);
            let ambiguous_one =
                new_test_tmux_command_pane(&socket_name, "local", Some(&unrelated), &shell);
            let ambiguous_two =
                new_test_tmux_command_pane(&socket_name, "local", Some(&unrelated), &shell);
            let failed =
                new_test_tmux_command_pane(&socket_name, "local", Some(&unrelated), &shell);
            for (index, pane_id) in [&recoverable, &ambiguous_one, &ambiguous_two, &failed]
                .into_iter()
                .enumerate()
            {
                let probe_path = fixture.path().join(format!("shell-ready-{index}"));
                wait_for_test_shell_probe(&socket_name, pane_id, &probe_path);
            }
            set_test_pane_title(
                &socket_name,
                &recoverable,
                "[mosh] remote-mac:~/Developer/Omu/recovered-project",
            );
            for pane_id in [&ambiguous_one, &ambiguous_two] {
                set_test_pane_title(
                    &socket_name,
                    pane_id,
                    "[mosh] remote-mac:~/Developer/Omu/ambiguous-project",
                );
            }
            set_test_pane_title(
                &socket_name,
                &failed,
                "[mosh] remote-mac:~/Developer/Omu/failed-project",
            );
            test_tmux_output(&socket_name, &["select-pane", "-t", &unrelated]);

            let config = Config {
                tmux_args: vec!["-L".into(), socket_name.clone()],
                ..Config::default()
            };
            let tmux = Tmux::new(&config);
            let mut record = remote_tmux_record("recovered-session", "recovered-project");
            record.cwd = "/Users/example/Developer/Omu/recovered-project".into();

            let mut named = remote_tmux_record("named-session", "recovered-project");
            named.server = "named".into();
            named.cwd = record.cwd.clone();
            assert_eq!(
                tmux.focus_agent(&named).unwrap(),
                FocusOutcome::TransportOnly
            );
            assert!(!log_path.exists());

            let mut missing = remote_tmux_record("missing-session", "missing-project");
            missing.cwd = "/Users/example/Developer/Omu/missing-project".into();
            assert_eq!(
                tmux.focus_agent(&missing).unwrap(),
                FocusOutcome::TransportOnly
            );

            let mut ambiguous = remote_tmux_record("ambiguous-session", "ambiguous-project");
            ambiguous.cwd = "/Users/example/Developer/Omu/ambiguous-project".into();
            let error = tmux.focus_agent(&ambiguous).unwrap_err();
            assert!(error.to_string().contains(&ambiguous_one));
            assert!(error.to_string().contains(&ambiguous_two));
            assert!(!log_path.exists());
            for pane_id in [&ambiguous_one, &ambiguous_two] {
                assert_eq!(
                    test_pane_option(&socket_name, pane_id, REMOTE_HOST_OPTION),
                    ""
                );
                assert_eq!(
                    test_pane_option(&socket_name, pane_id, REMOTE_SESSION_OPTION),
                    ""
                );
            }

            let mut failed_record = remote_tmux_record("failed-session", "failed-project");
            failed_record.cwd = "/Users/example/Developer/Omu/failed-project".into();
            let error = tmux.focus_agent(&failed_record).unwrap_err();
            assert!(error.to_string().contains("did not attach"));
            wait_for_test_file_contents(
                &log_path,
                "select-window -t @0\nselect-pane -t %0\nattach-session -t failed-session\n",
            );
            assert_eq!(
                test_pane_option(&socket_name, &failed, REMOTE_HOST_OPTION),
                ""
            );
            assert_eq!(
                test_pane_option(&socket_name, &failed, REMOTE_SESSION_OPTION),
                ""
            );

            assert_eq!(tmux.focus_agent(&record).unwrap(), FocusOutcome::Exact);

            assert_eq!(
                std::fs::read_to_string(&log_path).unwrap(),
                concat!(
                    "select-window -t @0\n",
                    "select-pane -t %0\n",
                    "attach-session -t failed-session\n",
                    "select-window -t @0\n",
                    "select-pane -t %0\n",
                    "attach-session -t recovered-session\n"
                )
            );
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &recoverable,
                        "#{pane_active}"
                    ],
                ),
                "1"
            );
            assert_eq!(
                test_pane_option(&socket_name, &recoverable, REMOTE_HOST_OPTION),
                "remote-mac"
            );
            assert_eq!(
                test_pane_option(&socket_name, &recoverable, REMOTE_SESSION_OPTION),
                "recovered-session"
            );
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!("tmux-agent-remote-attach-{}-{nonce}", std::process::id());
        let tmux_path = String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v tmux"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux::tests::remote_tmux_focus_recovers_a_detached_session_through_one_idle_mosh_shell",
                "--nocapture",
            ])
            .env(SOCKET_ENV, &socket_name)
            .env("TMUX_AGENT_TEST_TMUX", tmux_path.trim())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn remote_tmux_focus_adopts_a_running_mosh_session_in_another_window() {
        const SOCKET_ENV: &str = "TMUX_AGENT_RUNNING_REMOTE_FOCUS_TEST_SOCKET";
        if let Some(socket_name) = std::env::var_os(SOCKET_ENV) {
            let socket_name = socket_name.to_string_lossy().into_owned();
            let mosh_command = "/bin/bash -c 'exec -a \"mosh-client -# --no-init remote-mac | <address> <port>\" sleep 30'";
            let restored_mosh_command = "/bin/bash -c 'exec -a \"mosh-client -# --no-init remote-mac -- tmux attach-session -t work | <address> <port>\" sleep 30'";
            let wrong_host_command = "/bin/bash -c 'exec -a \"mosh-client -# --no-init other-remote -- tmux attach-session -t work | <address> <port>\" sleep 30'";
            let current = new_test_tmux_pane(&socket_name, "local", None);
            let other_session =
                new_test_tmux_command_pane(&socket_name, "local", Some(&current), mosh_command);
            mark_test_remote_pane(
                &socket_name,
                &other_session,
                "remote-mac",
                "other-session",
                "[mosh] · other-project",
            );
            let inferred_other = test_tmux_value(
                &socket_name,
                &[
                    "new-window",
                    "-d",
                    "-t",
                    "local",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    mosh_command,
                ],
            );
            wait_for_test_process_title(
                &socket_name,
                &inferred_other,
                "mosh-client -# --no-init remote-mac",
            );
            set_test_pane_title(&socket_name, &inferred_other, "[mosh] · other-project");
            let stale_cross_host = test_tmux_value(
                &socket_name,
                &[
                    "new-window",
                    "-d",
                    "-t",
                    "local",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    mosh_command,
                ],
            );
            wait_for_test_process_title(
                &socket_name,
                &stale_cross_host,
                "mosh-client -# --no-init remote-mac",
            );
            mark_test_remote_pane(
                &socket_name,
                &stale_cross_host,
                "old-remote",
                "old-project",
                "[mosh] · renamed-project",
            );
            let running = test_tmux_value(
                &socket_name,
                &[
                    "new-window",
                    "-d",
                    "-t",
                    "local",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    restored_mosh_command,
                ],
            );
            wait_for_test_process_title(
                &socket_name,
                &running,
                "mosh-client -# --no-init remote-mac -- tmux attach-session -t work",
            );
            set_test_pane_title(&socket_name, &running, "[mosh] · ⣹ sample-robot-project");
            let wrong_host = new_test_tmux_command_pane(
                &socket_name,
                "local",
                Some(&running),
                wrong_host_command,
            );
            wait_for_test_process_title(
                &socket_name,
                &wrong_host,
                "mosh-client -# --no-init other-remote -- tmux attach-session -t work",
            );
            set_test_pane_title(&socket_name, &wrong_host, "[mosh] · ⣹ sample-robot-project");
            test_tmux_output(&socket_name, &["select-window", "-t", "local:0"]);
            test_tmux_output(&socket_name, &["select-pane", "-t", &current]);

            let config = Config {
                tmux_args: vec!["-L".into(), socket_name.clone()],
                ..Config::default()
            };
            let tmux = Tmux::new(&config);

            tmux.focus_agent(&remote_tmux_record("other-session", "other-project"))
                .unwrap();
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &other_session,
                        "#{pane_active}"
                    ],
                ),
                "1"
            );
            assert_eq!(
                test_pane_option(&socket_name, &inferred_other, REMOTE_HOST_OPTION),
                ""
            );

            assert_eq!(
                tmux.focus_agent(&remote_tmux_record("new-session", "renamed-project"))
                    .unwrap(),
                FocusOutcome::TransportOnly
            );
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &stale_cross_host,
                        "#{pane_active}"
                    ],
                ),
                "1"
            );
            assert_eq!(
                test_pane_option(&socket_name, &stale_cross_host, REMOTE_HOST_OPTION),
                "remote-mac"
            );
            assert_eq!(
                test_pane_option(&socket_name, &stale_cross_host, REMOTE_SESSION_OPTION),
                "new-session"
            );

            assert_eq!(
                tmux.focus_agent(&remote_tmux_record(
                    "simulation - development",
                    "sample-robot-project",
                ))
                .unwrap(),
                FocusOutcome::TransportOnly
            );

            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &["display-message", "-p", "-t", &running, "#{pane_active}"],
                ),
                "1"
            );
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &["display-message", "-p", "-t", &running, "#{window_active}"],
                ),
                "1"
            );
            assert_eq!(
                test_pane_option(&socket_name, &running, REMOTE_HOST_OPTION),
                "remote-mac"
            );
            assert_eq!(
                test_pane_option(&socket_name, &running, REMOTE_SESSION_OPTION),
                "simulation - development"
            );
            assert_eq!(
                test_pane_option(&socket_name, &other_session, REMOTE_SESSION_OPTION),
                "other-session"
            );
            assert_eq!(
                test_pane_option(&socket_name, &wrong_host, REMOTE_HOST_OPTION),
                ""
            );
            assert_eq!(
                test_pane_option(&socket_name, &wrong_host, REMOTE_SESSION_OPTION),
                ""
            );
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!(
            "tmux-agent-running-remote-focus-{}-{nonce}",
            std::process::id()
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux::tests::remote_tmux_focus_adopts_a_running_mosh_session_in_another_window",
                "--nocapture",
            ])
            .env(SOCKET_ENV, &socket_name)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn remote_tmux_focus_does_not_adopt_ordinary_or_ambiguous_mosh_panes() {
        const SOCKET_ENV: &str = "TMUX_AGENT_RUNNING_REMOTE_AMBIGUITY_TEST_SOCKET";
        if let Some(socket_name) = std::env::var_os(SOCKET_ENV) {
            let socket_name = socket_name.to_string_lossy().into_owned();
            let mosh_command = "/bin/bash -c 'exec -a \"mosh-client -# --no-init remote-mac | <address> <port>\" sleep 30'";
            let current = new_test_tmux_pane(&socket_name, "local", None);
            let ordinary =
                new_test_tmux_command_pane(&socket_name, "local", Some(&current), mosh_command);
            let ambiguous_one = test_tmux_value(
                &socket_name,
                &[
                    "new-window",
                    "-d",
                    "-t",
                    "local",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    mosh_command,
                ],
            );
            let ambiguous_two = new_test_tmux_command_pane(
                &socket_name,
                "local",
                Some(&ambiguous_one),
                mosh_command,
            );
            for pane_id in [&ordinary, &ambiguous_one, &ambiguous_two] {
                wait_for_test_process_title(
                    &socket_name,
                    pane_id,
                    "mosh-client -# --no-init remote-mac",
                );
            }
            set_test_pane_title(&socket_name, &ordinary, "[mosh] ordinary-project");
            set_test_pane_title(&socket_name, &ambiguous_one, "[mosh] · ambiguous-project");
            set_test_pane_title(&socket_name, &ambiguous_two, "[mosh] · ambiguous-project");

            let config = Config {
                tmux_args: vec!["-L".into(), socket_name.clone()],
                ..Config::default()
            };
            let tmux = Tmux::new(&config);

            let error = tmux
                .focus_agent(&remote_tmux_record("ordinary-session", "ordinary-project"))
                .unwrap_err();
            assert!(is_focus_target_missing(&error));
            assert_eq!(
                test_pane_option(&socket_name, &ordinary, REMOTE_HOST_OPTION),
                ""
            );
            assert_eq!(
                test_pane_option(&socket_name, &ordinary, REMOTE_SESSION_OPTION),
                ""
            );

            let error = tmux
                .focus_agent(&remote_tmux_record(
                    "ambiguous-session",
                    "ambiguous-project",
                ))
                .unwrap_err();
            assert!(error.to_string().contains(&ambiguous_one));
            assert!(error.to_string().contains(&ambiguous_two));
            for pane_id in [&ambiguous_one, &ambiguous_two] {
                assert_eq!(
                    test_pane_option(&socket_name, pane_id, REMOTE_HOST_OPTION),
                    ""
                );
                assert_eq!(
                    test_pane_option(&socket_name, pane_id, REMOTE_SESSION_OPTION),
                    ""
                );
            }

            mark_test_remote_pane(
                &socket_name,
                &ambiguous_one,
                "old-remote",
                "old-first",
                "[mosh] · ambiguous-project",
            );
            mark_test_remote_pane(
                &socket_name,
                &ambiguous_two,
                "old-remote",
                "old-second",
                "[mosh] · ambiguous-project",
            );
            let error = tmux
                .focus_agent(&remote_tmux_record("fresh-stale", "ambiguous-project"))
                .unwrap_err();
            assert!(error.to_string().contains(&ambiguous_one));
            assert!(error.to_string().contains(&ambiguous_two));
            assert_eq!(
                test_pane_option(&socket_name, &ambiguous_one, REMOTE_HOST_OPTION),
                "old-remote"
            );
            assert_eq!(
                test_pane_option(&socket_name, &ambiguous_one, REMOTE_SESSION_OPTION),
                "old-first"
            );
            assert_eq!(
                test_pane_option(&socket_name, &ambiguous_two, REMOTE_HOST_OPTION),
                "old-remote"
            );
            assert_eq!(
                test_pane_option(&socket_name, &ambiguous_two, REMOTE_SESSION_OPTION),
                "old-second"
            );
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!(
            "tmux-agent-running-remote-ambiguity-{}-{nonce}",
            std::process::id()
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux::tests::remote_tmux_focus_does_not_adopt_ordinary_or_ambiguous_mosh_panes",
                "--nocapture",
            ])
            .env(SOCKET_ENV, &socket_name)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn remote_terminal_focus_resolves_a_unique_mosh_process_without_binding() {
        const SOCKET_ENV: &str = "TMUX_AGENT_REMOTE_TERMINAL_FOCUS_TEST_SOCKET";
        if let Some(socket_name) = std::env::var_os(SOCKET_ENV) {
            let socket_name = socket_name.to_string_lossy().into_owned();
            let mosh_command = |alias: &str| {
                format!(
                    "/bin/bash -c 'exec -a \"mosh-client -# --no-init {alias} | <address> <port>\" sleep 30'"
                )
            };
            let ssh_command = "/bin/bash -c 'exec -a \"ssh remote-mac\" sleep 30'".to_string();

            let unrelated = new_test_tmux_command_pane(
                &socket_name,
                "local",
                None,
                &mosh_command("other-remote"),
            );
            let unique = new_test_tmux_command_pane(
                &socket_name,
                "local",
                Some(&unrelated),
                &mosh_command("remote-mac"),
            );
            let marked = new_test_tmux_command_pane(
                &socket_name,
                "local",
                Some(&unrelated),
                &mosh_command("remote-mac"),
            );
            let ssh =
                new_test_tmux_command_pane(&socket_name, "local", Some(&unrelated), &ssh_command);
            let ambiguous_one = new_test_tmux_command_pane(
                &socket_name,
                "local",
                Some(&unrelated),
                &mosh_command("remote-mac"),
            );
            let ambiguous_two = new_test_tmux_command_pane(
                &socket_name,
                "local",
                Some(&unrelated),
                &mosh_command("remote-mac"),
            );

            set_test_pane_title(&socket_name, &unrelated, "[mosh] ⣸ project");
            set_test_pane_title(&socket_name, &unique, "[mosh] ⣸ project");
            mark_test_remote_pane(
                &socket_name,
                &marked,
                "remote-mac",
                "nested-session",
                "[mosh] ⣸ project",
            );
            set_test_pane_title(&socket_name, &ssh, "ssh-project");
            set_test_pane_title(&socket_name, &ambiguous_one, "[mosh] ⣴ ambiguous");
            set_test_pane_title(&socket_name, &ambiguous_two, "[mosh] ⣴ ambiguous");
            test_tmux_output(&socket_name, &["select-pane", "-t", &unrelated]);

            let config = Config {
                tmux_args: vec!["-L".into(), socket_name.clone()],
                ..Config::default()
            };
            let tmux = Tmux::new(&config);

            tmux.focus_agent(&remote_terminal_record("⣹ project"))
                .unwrap();
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &["display-message", "-p", "-t", &unique, "#{pane_active}"],
                ),
                "1"
            );
            assert_eq!(
                test_pane_option(&socket_name, &unique, REMOTE_HOST_OPTION),
                ""
            );
            assert_eq!(
                test_pane_option(&socket_name, &unique, REMOTE_SESSION_OPTION),
                ""
            );

            tmux.focus_agent(&remote_terminal_record("ssh-project"))
                .unwrap();
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &["display-message", "-p", "-t", &ssh, "#{pane_active}"],
                ),
                "1"
            );

            test_tmux_output(&socket_name, &["select-pane", "-t", &unrelated]);
            let error = tmux
                .focus_agent(&remote_terminal_record("ambiguous"))
                .unwrap_err();
            assert!(error.to_string().contains(&ambiguous_one));
            assert!(error.to_string().contains(&ambiguous_two));
            assert_eq!(
                test_tmux_value(
                    &socket_name,
                    &["display-message", "-p", "-t", &unrelated, "#{pane_active}",],
                ),
                "1"
            );
            for pane_id in [&ambiguous_one, &ambiguous_two] {
                assert_eq!(
                    test_pane_option(&socket_name, pane_id, REMOTE_HOST_OPTION),
                    ""
                );
                assert_eq!(
                    test_pane_option(&socket_name, pane_id, REMOTE_SESSION_OPTION),
                    ""
                );
            }
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!(
            "tmux-agent-remote-terminal-focus-{}-{nonce}",
            std::process::id()
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux::tests::remote_terminal_focus_resolves_a_unique_mosh_process_without_binding",
                "--nocapture",
            ])
            .env(SOCKET_ENV, &socket_name)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn parses_ps_rows_with_full_arguments() {
        let process = parse_process_with_terminal_resolver(
            "  502 123 10 123 123 ttys003 01:02 /usr/bin/codex --model smart",
            named_terminal,
        )
        .unwrap();
        assert_eq!(process.uid, 502);
        assert_eq!(process.pid, 123);
        assert_eq!(process.parent_pid, 10);
        assert_eq!(process.process_group, 123);
        assert_eq!(process.foreground_group, Some(123));
        assert_eq!(process.terminal.as_deref(), Some("ttys003"));
        assert_eq!(process.args, "/usr/bin/codex --model smart");
        assert_eq!(parse_elapsed_ms("01:02"), Some(62_000));
        assert_eq!(parse_elapsed_ms("02:03:04"), Some(7_384_000));
        assert_eq!(parse_elapsed_ms("1-02:03:04"), Some(93_784_000));
    }

    #[test]
    fn process_parser_applies_an_independent_terminal_fixture() {
        let process = parse_process_with_terminal_resolver(
            "  502 123 10 123 123 16/3 01:02 /usr/bin/codex --model smart",
            |terminal| {
                assert_eq!(terminal, "16/3");
                Some("ttys003".into())
            },
        )
        .unwrap();

        assert_eq!(process.terminal.as_deref(), Some("ttys003"));
        assert_eq!(process.args, "/usr/bin/codex --model smart");
    }

    #[test]
    fn process_inventory_cache_refreshes_after_one_second_and_reprojects_panes() {
        fn process(pid: u32, process_group: u32, foreground_group: u32, args: &str) -> Process {
            Process {
                uid: unsafe { libc::geteuid() },
                pid,
                parent_pid: 1,
                process_group,
                foreground_group: Some(foreground_group),
                terminal: None,
                args: args.into(),
            }
        }

        fn inventory() -> ProcessInventory {
            ProcessInventory {
                processes: vec![
                    process(100, 100, 200, "shell"),
                    process(200, 200, 200, "codex --model smart"),
                    process(300, 300, 400, "shell"),
                    process(400, 400, 400, "claude --model fast"),
                ],
                process_started_at_ms: HashMap::new(),
                tcp_connections: HashMap::new(),
                parent_pids: HashMap::new(),
                ssh_connections: HashMap::new(),
                client_connections: HashMap::new(),
            }
        }

        let tmux = Tmux::new(&Config::default());
        let base = std::time::Instant::now();
        let moments = std::cell::RefCell::new(std::collections::VecDeque::from([
            base,
            base + std::time::Duration::from_millis(750),
            base + std::time::Duration::from_millis(900),
            base + std::time::Duration::from_millis(1_750),
            base + std::time::Duration::from_millis(2_500),
        ]));
        let mut now = || moments.borrow_mut().pop_front().unwrap();
        let refreshes = std::cell::Cell::new(0);
        let mut current_pane = pane("%1", "main", "agent");
        current_pane.pane_pid = 100;

        let first = tmux
            .process_snapshot_with(&[current_pane.clone()], &mut now, || {
                refreshes.set(refreshes.get() + 1);
                Ok(inventory())
            })
            .unwrap();
        assert_eq!(first.panes["%1"], "codex --model smart");
        assert_eq!(refreshes.get(), 1);

        current_pane.pane_pid = 300;
        let cached = tmux
            .process_snapshot_with(&[current_pane.clone()], &mut now, || {
                panic!("inventory refreshed inside its one-second lifetime")
            })
            .unwrap();
        assert_eq!(cached.panes["%1"], "claude --model fast");
        assert_eq!(refreshes.get(), 1);

        let refreshed = tmux
            .process_snapshot_with(&[current_pane], &mut now, || {
                refreshes.set(refreshes.get() + 1);
                Ok(inventory())
            })
            .unwrap();
        assert_eq!(refreshed.panes["%1"], "claude --model fast");
        assert_eq!(refreshes.get(), 2);
        assert!(moments.borrow().is_empty());
    }

    #[test]
    fn expired_process_inventory_refresh_error_is_not_hidden_by_stale_data() {
        fn empty_inventory() -> ProcessInventory {
            ProcessInventory {
                processes: Vec::new(),
                process_started_at_ms: HashMap::new(),
                tcp_connections: HashMap::new(),
                parent_pids: HashMap::new(),
                ssh_connections: HashMap::new(),
                client_connections: HashMap::new(),
            }
        }

        let tmux = Tmux::new(&Config::default());
        let base = Instant::now();
        let moments = std::cell::RefCell::new(std::collections::VecDeque::from([
            base,
            base,
            base + Duration::from_secs(1),
            base + Duration::from_millis(1_001),
            base + Duration::from_millis(1_001),
        ]));
        let mut now = || moments.borrow_mut().pop_front().unwrap();
        tmux.process_snapshot_with(&[], &mut now, || Ok(empty_inventory()))
            .unwrap();

        let error = tmux
            .process_snapshot_with(&[], &mut now, || {
                Err(anyhow::anyhow!("process inventory failed"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "process inventory failed");

        tmux.process_snapshot_with(&[], &mut now, || Ok(empty_inventory()))
            .unwrap();
        assert!(moments.borrow().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_process_columns_request_numeric_terminal_devices() {
        assert!(PROCESS_COLUMNS.contains("tdev="));
        assert!(!PROCESS_COLUMNS.contains("tty="));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminal_device_fixture_maps_back_to_the_tty_name() {
        let terminal = resolve_macos_terminal_with("16/3", |device| {
            assert_eq!(libc::major(device), 16);
            assert_eq!(libc::minor(device), 3);
            Some("ttys003".into())
        });

        assert_eq!(terminal.as_deref(), Some("ttys003"));
        assert!(resolve_macos_terminal_with("??", |_| unreachable!()).is_none());
        assert!(resolve_macos_terminal_with("16/not-a-number", |_| unreachable!()).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminal_names_persist_across_process_snapshots() {
        let cache = MacosTerminalNames::default();
        let snapshots = [
            "502 123 10 123 123 16/3 00:01 codex\n502 124 10 124 124 16/4 00:01 shell",
            "502 223 10 223 223 16/3 00:01 codex\n502 224 10 224 224 16/4 00:01 shell",
        ];
        let mut lookups = Vec::new();

        let parsed = snapshots.map(|snapshot| {
            parse_macos_processes_with(snapshot, &cache, |device| {
                lookups.push((libc::major(device), libc::minor(device)));
                (libc::minor(device) == 3).then(|| "ttys003".into())
            })
        });

        assert_eq!(lookups, [(16, 3), (16, 4)]);
        for snapshot in parsed {
            assert_eq!(snapshot[0].terminal.as_deref(), Some("ttys003"));
            assert_eq!(snapshot[1].terminal, None);
        }
    }

    #[test]
    fn pane_metadata_does_not_depend_on_host_presentation_options() {
        let separator = SEPARATOR.to_string();
        let line = [
            "%1",
            "123",
            "$1",
            "main",
            "@1",
            "1",
            "work",
            "0",
            "codex",
            "title",
            "/tmp",
            "0",
            "1",
            "1",
            "1",
            "",
            "remote-host",
            "remote-session",
            "remote development",
        ]
        .join(&separator);
        let pane = parse_pane(&line).unwrap();
        assert!(pane.visible);
        assert!(!pane.dead);
        assert_eq!(pane.mirror_host.as_deref(), Some("remote-host"));
        assert_eq!(pane.mirror_session.as_deref(), Some("remote-session"));
        assert_eq!(pane.label.as_deref(), Some("remote development"));
    }

    #[test]
    fn pane_visibility_requires_an_active_window_and_attached_session() {
        assert!(pane_is_visible("1", "1"));
        assert!(pane_is_visible("1", "2"));
        assert!(!pane_is_visible("0", "1"));
        assert!(!pane_is_visible("1", "0"));
    }

    #[test]
    fn selection_broadcast_targets_every_live_ui_pane() {
        let mut first = pane("%1", "one", "ui");
        first.is_agent_ui = true;
        let mut second = pane("%2", "two", "ui");
        second.is_agent_ui = true;
        let ordinary = pane("%3", "three", "shell");
        let mut dead = pane("%4", "four", "ui");
        dead.is_agent_ui = true;
        dead.dead = true;

        assert_eq!(
            selection_broadcast_panes(&[first, second, ordinary, dead]),
            vec!["%1", "%2"]
        );
    }

    #[test]
    fn selection_broadcast_continues_after_one_pane_disappears() {
        let pane_ids = vec!["%1".to_string(), "%2".to_string(), "%3".to_string()];
        let attempted = std::sync::Mutex::new(Vec::new());

        let error = wake_ui_panes(&pane_ids, |pane_id| {
            attempted.lock().unwrap().push(pane_id.to_string());
            if pane_id == "%2" {
                anyhow::bail!("pane disappeared");
            }
            Ok(())
        })
        .unwrap_err();

        let mut attempted = attempted.into_inner().unwrap();
        attempted.sort();
        assert_eq!(attempted, vec!["%1", "%2", "%3"]);
        assert!(error.to_string().contains("%2"));
    }

    #[test]
    fn parses_raw_and_escaped_pane_visibility_records() {
        assert!(parse_pane_visibility("1\u{1f}2\n", "%1").unwrap());
        assert!(parse_pane_visibility(r"1\0372", "%1").unwrap());

        let error = parse_pane_visibility("1", "%1").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unexpected tmux visibility record for %1 with 1 fields")
        );
    }

    #[test]
    fn parses_pane_separators_escaped_by_older_tmux() {
        let line = [
            "%1", "123", "$1", "main", "@1", "1", "work", "0", "codex", "title", "/tmp", "0", "1",
            "1", "1", "", "", "", "",
        ]
        .join(ESCAPED_SEPARATOR);

        let pane = parse_pane(&line).unwrap();

        assert_eq!(pane.pane_id, "%1");
        assert_eq!(pane.pane_pid, 123);
        assert!(pane.visible);
    }

    #[test]
    fn parses_dead_panes() {
        let separator = SEPARATOR.to_string();
        let line = [
            "%1", "123", "$1", "main", "@1", "1", "work", "0", "codex", "title", "/tmp", "1", "0",
            "1", "1", "", "", "", "",
        ]
        .join(&separator);
        let pane = parse_pane(&line).unwrap();
        assert!(pane.dead);
    }

    #[test]
    fn parses_remote_focus_markers() {
        let separator = SEPARATOR.to_string();
        let preferred = [
            "%1",
            "123",
            "$1",
            "main",
            "@1",
            "1",
            "work",
            "0",
            "ssh",
            "title",
            "/tmp",
            "0",
            "1",
            "1",
            "1",
            "",
            "remote-mac",
            "project",
            "",
        ]
        .join(&separator);

        let preferred = parse_pane(&preferred).unwrap();

        assert_eq!(preferred.mirror_host.as_deref(), Some("remote-mac"));
        assert_eq!(preferred.mirror_session.as_deref(), Some("project"));
    }

    #[test]
    fn recognizes_missing_tmux_server_errors() {
        assert!(is_missing_server("server exited unexpectedly"));
        assert!(is_missing_server(
            "error connecting to /private/tmp/tmux-502/default (No such file or directory)"
        ));
        assert!(is_missing_server(
            "no server running on /private/tmp/tmux-502/default"
        ));
        assert!(!is_missing_server("unknown command: list-pnaes"));
    }

    #[test]
    fn runtime_key_scopes_daemons_to_the_selected_tmux_server() {
        assert_eq!(runtime_key_for(&[], None, None, 501), "default");
        assert_eq!(
            runtime_key_for(
                &[],
                Some("/tmp/tmux-501/default,123,0"),
                Some(OsStr::new("/tmp")),
                501
            ),
            "default"
        );
        assert_eq!(
            runtime_key_for(
                &[],
                Some("/tmp/tmux-501/agents,123,0"),
                Some(OsStr::new("/tmp")),
                501
            ),
            "tmux-env:/tmp/tmux-501/agents"
        );
        assert_eq!(
            runtime_key_for(&["-L".into(), "agents".into()], Some("ignored"), None, 501),
            "tmux:-L\u{1f}agents"
        );
    }

    #[test]
    fn custom_sockets_named_default_keep_distinct_runtime_keys() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first/default");
        let second = directory.path().join("second/default");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, []).unwrap();
        fs::write(&second, []).unwrap();

        let first_environment = format!("{},123,0", first.display());
        let second_environment = format!("{},456,0", second.display());
        let first_key = runtime_key_for(&[], Some(&first_environment), None, 501);
        let second_key = runtime_key_for(&[], Some(&second_environment), None, 501);

        assert_ne!(first_key, second_key);
        assert_eq!(first_key, format!("tmux-env:{}", first.display()));
        assert_eq!(second_key, format!("tmux-env:{}", second.display()));
    }

    #[test]
    fn capture_uses_only_the_live_visible_pane() {
        let framing = CaptureBatchFraming {
            nonce: "fixture".into(),
        };
        let args = capture_batch_args(&["%42".into()], &framing);
        let capture = args
            .iter()
            .find(|arg| arg.starts_with("capture-pane"))
            .unwrap();
        assert_eq!(capture, "capture-pane -p -t %42");
        assert!(!capture.contains(" -S"));
        assert!(!capture.contains(" -M"));
    }

    #[test]
    fn batched_capture_preserves_successes_when_one_pane_disappears() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!("tmux-agent-capture-{}-{nonce}", std::process::id());
        let config = Config {
            tmux_args: vec!["-L".into(), socket_name.clone()],
            ..Config::default()
        };
        let started = Command::new("tmux")
            .args([
                "-L",
                &socket_name,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "capture-test",
                "printf 'first-pane\\n'; sleep 30",
            ])
            .status()
            .unwrap();
        assert!(started.success());
        let split = Command::new("tmux")
            .args([
                "-L",
                &socket_name,
                "split-window",
                "-d",
                "-t",
                "capture-test",
                "printf 'second-pane\\n'; sleep 30",
            ])
            .status()
            .unwrap();
        assert!(split.success());
        let pane_output = Command::new("tmux")
            .args([
                "-L",
                &socket_name,
                "list-panes",
                "-t",
                "capture-test",
                "-F",
                "#{pane_id}",
            ])
            .output()
            .unwrap();
        assert!(pane_output.status.success());
        let pane_ids = String::from_utf8(pane_output.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(pane_ids.len(), 2);
        for (pane_id, expected) in pane_ids.iter().zip(["first-pane", "second-pane"]) {
            let ready = (0..100).any(|_| {
                let output = Command::new("tmux")
                    .args(["-L", &socket_name, "capture-pane", "-p", "-t", pane_id])
                    .output()
                    .unwrap();
                if String::from_utf8_lossy(&output.stdout).contains(expected) {
                    true
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    false
                }
            });
            assert!(ready, "pane {pane_id} did not render {expected}");
        }
        let missing = "%999999".to_string();
        let requested = vec![pane_ids[0].clone(), missing.clone(), pane_ids[1].clone()];

        let captures = Tmux::new(&config).capture_visible_batch(&requested);

        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert!(
            captures[&pane_ids[0]]
                .as_ref()
                .unwrap()
                .contains("first-pane"),
            "first capture was {:?}",
            captures[&pane_ids[0]]
        );
        assert!(
            captures[&pane_ids[1]]
                .as_ref()
                .unwrap()
                .contains("second-pane"),
            "second capture was {:?}",
            captures[&pane_ids[1]]
        );
        assert!(captures[&missing].is_err());
    }

    #[test]
    fn focus_only_switches_a_current_tmux_client() {
        assert!(!has_current_tmux_client(None));
        assert!(!has_current_tmux_client(Some(std::ffi::OsStr::new(""))));
        assert!(has_current_tmux_client(Some(std::ffi::OsStr::new(
            "/tmp/tmux,1,0"
        ))));
    }

    #[test]
    fn remote_title_resolves_to_unique_local_ssh_pane() {
        let panes = [
            pane("%45", "project-one", "project-one"),
            pane("%46", "project-two", "⠸ project-two"),
        ];
        let processes = HashMap::from([
            ("%45".to_string(), "ssh remote-mac".to_string()),
            ("%46".to_string(), "/usr/bin/ssh -tt remote-mac".to_string()),
        ]);
        assert_eq!(
            find_transport_pane(&panes, &processes, "remote-mac", "project-one")
                .unwrap()
                .map(|pane| pane.pane_id.as_str()),
            Some("%45")
        );
        assert_eq!(
            find_transport_pane(&panes, &processes, "remote-mac", "project-two")
                .unwrap()
                .map(|pane| pane.pane_id.as_str()),
            Some("%46")
        );
    }

    #[test]
    fn remote_terminal_title_does_not_claim_an_explicit_session_mirror() {
        let mut marked = pane("%63", "walkme", "walk-me-through-the-code");
        marked.mirror_host = Some("remote-mac".into());
        marked.mirror_session = Some("wmtc-manual-48".into());
        let panes = [marked];
        let processes = HashMap::from([("%63".to_string(), "ssh remote-mac".to_string())]);

        assert!(
            find_transport_pane(&panes, &processes, "remote-mac", "walk-me-through-the-code")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_remote_session_selects_the_marked_pane_over_a_matching_title() {
        let title_match = pane("%61", "walkme", "walk-me-through-the-code");
        let mut marked = pane(
            "%63",
            "walkme",
            "remote.local:wmtc-manual-48:0:node:walk-me-through-the-code",
        );
        marked.mirror_host = Some("remote-mac".into());
        marked.mirror_session = Some("wmtc-manual-48".into());
        let panes = [title_match, marked];

        assert_eq!(
            find_mirror_pane(&panes, "remote-mac", "wmtc-manual-48")
                .unwrap()
                .map(|pane| pane.pane_id.as_str()),
            Some("%63")
        );
    }

    #[test]
    fn duplicate_remote_session_markers_are_not_guessed() {
        let panes = ["%63", "%64"].map(|pane_id| {
            let mut pane = pane(pane_id, "walkme", "remote session");
            pane.mirror_host = Some("remote-mac".into());
            pane.mirror_session = Some("wmtc-manual-48".into());
            pane
        });

        let error = find_mirror_pane(&panes, "remote-mac", "wmtc-manual-48").unwrap_err();
        assert!(error.to_string().contains("%63, %64"));
    }

    #[test]
    fn host_transport_fallback_rejects_ambiguous_bindings() {
        let panes = ["%63", "%64"].map(|pane_id| {
            let mut pane = pane(pane_id, "local", "remote session");
            pane.mirror_host = Some("remote-mac".into());
            pane.mirror_session = Some(format!("session-{pane_id}"));
            pane
        });

        let error = find_bound_host_transport(&panes, "remote-mac").unwrap_err();

        assert!(error.to_string().contains("remote-mac"));
        assert!(error.to_string().contains("%63, %64"));
    }

    #[test]
    fn host_transport_fallback_excludes_dead_ui_and_partial_bindings() {
        let mut dead = pane("%dead", "local", "dead");
        dead.dead = true;
        dead.mirror_host = Some("remote-mac".into());
        dead.mirror_session = Some("dead-session".into());

        let mut ui = pane("%ui", "local", "ui");
        ui.is_agent_ui = true;
        ui.mirror_host = Some("remote-mac".into());
        ui.mirror_session = Some("ui-session".into());

        let mut host_only = pane("%host", "local", "host only");
        host_only.mirror_host = Some("remote-mac".into());

        let mut session_only = pane("%session", "local", "session only");
        session_only.mirror_session = Some("remote-session".into());

        let mut eligible = pane("%eligible", "local", "eligible");
        eligible.mirror_host = Some("remote-mac".into());
        eligible.mirror_session = Some("other-session".into());

        let excluded = [dead, ui, host_only, session_only];
        assert!(
            find_bound_host_transport(&excluded, "remote-mac")
                .unwrap()
                .is_none()
        );

        let mut panes = excluded.to_vec();
        panes.push(eligible);
        assert_eq!(
            find_bound_host_transport(&panes, "remote-mac")
                .unwrap()
                .map(|pane| pane.pane_id.as_str()),
            Some("%eligible")
        );
    }

    #[test]
    fn transport_titles_ignore_provider_braille_spinners() {
        assert_eq!(normalize_transport_title("⠂ project"), "project");
        assert_eq!(normalize_transport_title("⠸ project"), "project");
        assert_eq!(normalize_transport_title("[mosh] ⠸ project"), "project");
        assert_eq!(normalize_transport_title("[mosh] · ⣹ project"), "project");
        assert_eq!(normalize_transport_title("⠸"), "");
        assert_eq!(normalize_transport_title("⣿"), "⣿");
        assert_eq!(normalize_transport_title("⣿art"), "⣿art");
        assert_eq!(normalize_transport_title("✳ project"), "✳ project");
    }

    #[test]
    fn local_transport_keeps_label_session_and_canonical_host() {
        let mut pane = pane("%45", "local-session", "⠸ project-one");
        pane.label = Some("testing env".into());
        pane.mirror_host = Some("remote.example.test".into());
        pane.mirror_session = Some("remote-session".into());
        let aliases =
            HashMap::from([("remote.example.test".to_string(), "remote-mac".to_string())]);
        let connection = SshConnection {
            client_address: "192.0.2.10".into(),
            client_port: 64308,
            server_address: "198.51.100.20".into(),
            server_port: 22,
        };

        let transport = local_ssh_transport(
            &pane,
            "remote.example.test",
            Some(connection.clone()),
            &aliases,
        );

        assert_eq!(transport.connection, Some(connection));
        assert_eq!(transport.remote_host, "remote-mac");
        assert!(transport.remote_host_explicit);
        assert_eq!(transport.remote_session.as_deref(), Some("remote-session"));
        assert_eq!(transport.title, "project-one");
        assert_eq!(transport.label.as_deref(), Some("testing env"));
        assert!(transport.visible);
        assert_eq!(transport.target.pane_id, "%45");
    }

    #[test]
    fn local_transport_preserves_partial_marker_state() {
        let aliases =
            HashMap::from([("remote.example.test".to_string(), "remote-mac".to_string())]);

        let mut host_only = pane("%45", "local-session", "project-one");
        host_only.mirror_host = Some("remote-mac".into());
        let host_transport = local_ssh_transport(
            &host_only,
            host_only.mirror_host.as_deref().unwrap(),
            None,
            &aliases,
        );
        assert!(host_transport.remote_host_explicit);
        assert!(host_transport.remote_session.is_none());

        let mut session_only = pane("%46", "local-session", "project-two");
        session_only.mirror_session = Some("remote-session".into());
        let session_transport =
            local_ssh_transport(&session_only, "remote.example.test", None, &aliases);
        assert!(!session_transport.remote_host_explicit);
        assert_eq!(
            session_transport.remote_session.as_deref(),
            Some("remote-session")
        );
    }

    #[test]
    fn remote_title_does_not_guess_between_duplicate_ssh_panes() {
        let panes = [
            pane("%45", "one", "same-project"),
            pane("%46", "two", "same-project"),
        ];
        let processes = HashMap::from([
            ("%45".to_string(), "ssh remote-mac".to_string()),
            ("%46".to_string(), "ssh remote-mac".to_string()),
        ]);
        let error =
            find_transport_pane(&panes, &processes, "remote-mac", "same-project").unwrap_err();
        assert!(error.to_string().contains("%45, %46"));
    }

    #[test]
    fn transport_destination_preserves_ssh_options_and_user_prefix() {
        assert_eq!(
            transport_destination("/usr/bin/ssh -tt -o BatchMode=yes -p 22 agent@host"),
            Some("host")
        );
        assert_eq!(transport_destination("ssh -vJ jump remote"), Some("remote"));
        assert_eq!(transport_destination("ssh -vJjump remote"), Some("remote"));
        assert_eq!(transport_destination("mosh remote-mac"), None);
        assert_eq!(transport_destination("ssh -p 22"), None);
    }

    #[test]
    fn transport_destination_requires_the_foreground_group_leader() {
        assert_eq!(transport_destination("git fetch\nssh remote-mac"), None);
        assert_eq!(
            transport_destination("ssh remote-mac\nhelper process"),
            Some("remote-mac")
        );
    }

    #[test]
    fn mosh_destination_comes_from_the_client_process_title() {
        assert_eq!(
            transport_destination(
                "mosh-client -# --no-init remote-mac | <address> <port>\nhelper process"
            ),
            Some("remote-mac")
        );
        assert_eq!(
            transport_destination(
                "/usr/local/bin/mosh-client -# --no-init user@remote-mac | <address> <port>"
            ),
            Some("remote-mac")
        );
        assert_eq!(
            transport_destination("mosh-client -# --no-init -- build-host | <address> <port>"),
            Some("build-host")
        );
        assert_eq!(
            transport_destination("mosh-client -# --no-init -- user@build-host | <address> <port>"),
            Some("build-host")
        );
        assert_eq!(
            transport_destination("mosh-client -# -p 60000 -- build-host | <address> <port>"),
            Some("build-host")
        );
        assert_eq!(
            transport_destination("mosh-client -# -p 60000 -- user@build-host | <address> <port>"),
            Some("build-host")
        );
        assert_eq!(
            transport_destination("mosh-client -# -p=60000 -- build-host | <address> <port>"),
            Some("build-host")
        );
        assert_eq!(
            transport_destination(
                "mosh-client -# --ssh=ssh -J jump -- build-host | <address> <port>"
            ),
            None
        );
        assert_eq!(
            transport_destination(
                "mosh-client -# -pretend build-host -- tmux attach-session | <address> <port>"
            ),
            None
        );
        assert_eq!(
            transport_destination(
                "mosh-client -# build-host -- tmux attach-session -t work | <address> <port>"
            ),
            Some("build-host")
        );
        assert_eq!(
            transport_destination(
                "mosh-client -# user@build-host -- tmux attach-session -t work | <address> <port>"
            ),
            Some("build-host")
        );
        assert_eq!(
            transport_destination("mosh-client -# build-host -- | <address> <port>"),
            None
        );
        assert_eq!(
            transport_destination(
                "mosh-client -# build-host -- tmux -- attach-session | <address> <port>"
            ),
            None
        );
        assert_eq!(transport_destination("mosh remote-mac"), None);
        assert_eq!(
            transport_destination("mosh-client -# --no-init remote-mac | <address>"),
            None
        );
    }

    #[test]
    fn only_explicit_focus_misses_are_classified_as_missing() {
        let missing = anyhow::Error::new(FocusTargetMissing {
            alias: "remote-mac".into(),
            title: "project".into(),
            session: None,
            live_session: false,
        });
        assert!(is_focus_target_missing(&missing));
        assert!(!is_focus_target_missing(&anyhow::anyhow!("tmux failed")));
    }

    #[test]
    fn missing_remote_tmux_binding_reports_the_exact_bind_command() {
        let missing = FocusTargetMissing {
            alias: "thinkcat".into(),
            title: "project".into(),
            session: Some("tmux-agent-res".into()),
            live_session: false,
        };

        assert_eq!(
            missing.to_string(),
            "no local pane is bound to thinkcat/tmux-agent-res; run tmux-agent remote bind thinkcat tmux-agent-res --pane <local-pane-id> on this machine"
        );
    }

    #[test]
    fn idle_shell_excludes_old_agent_descendants() {
        let processes = [
            Process {
                uid: 502,
                pid: 5486,
                parent_pid: 50,
                process_group: 5486,
                foreground_group: Some(5486),
                terminal: Some("ttys001".into()),
                args: "/bin/zsh".into(),
            },
            Process {
                uid: 502,
                pid: 6000,
                parent_pid: 5486,
                process_group: 6000,
                foreground_group: Some(5486),
                terminal: Some("ttys001".into()),
                args: "/opt/homebrew/bin/grok".into(),
            },
        ];
        assert_eq!(
            foreground_job(5486, &processes)
                .map(|(_, args, _)| args)
                .as_deref(),
            Some("/bin/zsh")
        );
    }

    #[test]
    fn active_agent_foreground_group_is_selected() {
        let processes = [
            Process {
                uid: 502,
                pid: 5984,
                parent_pid: 50,
                process_group: 5984,
                foreground_group: Some(85312),
                terminal: Some("ttys002".into()),
                args: "/bin/zsh".into(),
            },
            Process {
                uid: 502,
                pid: 85312,
                parent_pid: 5984,
                process_group: 85312,
                foreground_group: Some(85312),
                terminal: Some("ttys002".into()),
                args: "/opt/homebrew/bin/grok".into(),
            },
        ];
        assert_eq!(
            foreground_job(5984, &processes)
                .map(|(_, args, _)| args)
                .as_deref(),
            Some("/opt/homebrew/bin/grok")
        );
    }

    #[test]
    fn discovers_foreground_terminal_job_once() {
        let processes = [
            Process {
                uid: 502,
                pid: 100,
                parent_pid: 1,
                process_group: 100,
                foreground_group: Some(200),
                terminal: Some("ttys003".into()),
                args: "-zsh".into(),
            },
            Process {
                uid: 502,
                pid: 200,
                parent_pid: 100,
                process_group: 200,
                foreground_group: Some(200),
                terminal: Some("ttys003".into()),
                args: "node /opt/homebrew/bin/codex".into(),
            },
            Process {
                uid: 502,
                pid: 201,
                parent_pid: 200,
                process_group: 200,
                foreground_group: Some(200),
                terminal: Some("ttys003".into()),
                args: "/opt/homebrew/lib/codex".into(),
            },
        ];
        let jobs = foreground_terminal_jobs(&processes, &HashSet::new(), &HashSet::new());
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "ttys003");
        assert_eq!(jobs[0].process_group, 200);
        assert_eq!(jobs[0].leader_pid, 200);
        assert_eq!(jobs[0].pids, [200, 201]);
        assert!(jobs[0].processes.contains("/opt/homebrew/bin/codex"));
    }

    #[test]
    fn excludes_tmux_and_detached_background_groups() {
        let processes = [
            Process {
                uid: 502,
                pid: 200,
                parent_pid: 100,
                process_group: 200,
                foreground_group: Some(200),
                terminal: Some("ttys003".into()),
                args: "/opt/homebrew/bin/codex".into(),
            },
            Process {
                uid: 502,
                pid: 300,
                parent_pid: 1,
                process_group: 300,
                foreground_group: None,
                terminal: None,
                args: "/opt/homebrew/bin/codex app-server".into(),
            },
        ];
        assert!(
            foreground_terminal_jobs(&processes, &HashSet::from([200]), &HashSet::new()).is_empty()
        );
    }

    #[test]
    fn excludes_primary_tmux_terminal_but_keeps_descendant_background_ptys() {
        let processes = [
            Process {
                uid: 502,
                pid: 100,
                parent_pid: 50,
                process_group: 100,
                foreground_group: Some(200),
                terminal: Some("ttys100".into()),
                args: "/bin/zsh".into(),
            },
            Process {
                uid: 502,
                pid: 200,
                parent_pid: 100,
                process_group: 200,
                foreground_group: Some(200),
                terminal: Some("ttys100".into()),
                args: "/opt/homebrew/bin/codex".into(),
            },
            Process {
                uid: 502,
                pid: 300,
                parent_pid: 200,
                process_group: 300,
                foreground_group: Some(300),
                terminal: Some("ttys101".into()),
                args: "/opt/homebrew/bin/codex exec review".into(),
            },
        ];
        let panes = [pane("%1", "main", "work")];
        let excluded = tmux_pane_terminals(&panes, &processes);
        assert_eq!(excluded, HashSet::from(["ttys100".to_string()]));
        let jobs = foreground_terminal_jobs(&processes, &HashSet::from([200]), &excluded);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "ttys101");
        assert_eq!(jobs[0].leader_pid, 300);
    }

    #[test]
    fn parses_lsof_working_directories() {
        let directories = parse_lsof_working_directories(
            b"p11265\nfcwd\nn/Users/agent/projects/remote-cli\n\
              p19788\nfcwd\nn/Users/agent/projects/sample-project\n",
        );
        assert_eq!(
            directories.get(&11265).map(String::as_str),
            Some("/Users/agent/projects/remote-cli")
        );
        assert_eq!(
            directories.get(&19788).map(String::as_str),
            Some("/Users/agent/projects/sample-project")
        );
    }

    #[test]
    fn parses_and_deduplicates_established_tcp_connections() {
        let connections = parse_lsof_tcp_connections(
            b"p34048\nf3\nn192.0.2.10:64308->198.51.100.20:22\n\
              p21906\nf7\nn198.51.100.20:22->192.0.2.10:64308\n\
              f8\nn198.51.100.20:22->192.0.2.10:64308\n",
        );
        assert_eq!(connections[&34048].len(), 1);
        assert_eq!(
            connections[&34048][0].connection,
            TcpConnection {
                left: TcpEndpoint {
                    address: "192.0.2.10".into(),
                    port: 64308,
                },
                right: TcpEndpoint {
                    address: "198.51.100.20".into(),
                    port: 22,
                },
            }
        );
        assert_eq!(connections[&21906].len(), 2);
        assert_eq!(
            ssh_transport_connection(&connections[&21906]),
            Some(TcpConnection {
                left: TcpEndpoint {
                    address: "198.51.100.20".into(),
                    port: 22,
                },
                right: TcpEndpoint {
                    address: "192.0.2.10".into(),
                    port: 64308,
                },
            })
        );
    }

    #[test]
    fn ssh_transport_uses_inherited_socket_before_forwarded_channels() {
        let sockets = parse_lsof_tcp_connections(
            b"p21906\n\
              f7\nn198.51.100.20:22->192.0.2.10:64308\n\
              f12\nn198.51.100.20:53122->203.0.113.55:443\n",
        );
        let connection = ssh_transport_connection(&sockets[&21906]).unwrap();
        assert_eq!(connection.left.port, 22);
        assert_eq!(connection.right.port, 64308);
    }

    #[test]
    fn parses_ipv6_tcp_endpoints() {
        assert_eq!(
            parse_tcp_connection("[fd7a:115c:a1e0::1]:51234->[fd7a:115c:a1e0::2]:22"),
            Some(TcpConnection {
                left: TcpEndpoint {
                    address: "fd7a:115c:a1e0::1".into(),
                    port: 51234,
                },
                right: TcpEndpoint {
                    address: "fd7a:115c:a1e0::2".into(),
                    port: 22,
                },
            })
        );
    }

    #[test]
    fn recognizes_cross_platform_sshd_session_names() {
        assert!(is_sshd_session_program("sshd-session: agent@ttys004"));
        assert!(is_sshd_session_program("sshd: user@pts/0"));
        assert!(is_sshd_session_program("/usr/sbin/sshd: user@pts/0"));
        assert!(!is_sshd_session_program("ssh remote-mac"));
    }

    #[test]
    fn multiplexed_ssh_ttys_do_not_resolve_to_the_control_master() {
        let processes = [
            Process {
                uid: 502,
                pid: 50,
                parent_pid: 1,
                process_group: 50,
                foreground_group: None,
                terminal: None,
                args: "sshd: user".into(),
            },
            Process {
                uid: 502,
                pid: 100,
                parent_pid: 50,
                process_group: 100,
                foreground_group: Some(200),
                terminal: Some("ttys001".into()),
                args: "-zsh".into(),
            },
            Process {
                uid: 502,
                pid: 200,
                parent_pid: 100,
                process_group: 200,
                foreground_group: Some(200),
                terminal: Some("ttys001".into()),
                args: "codex".into(),
            },
            Process {
                uid: 502,
                pid: 101,
                parent_pid: 50,
                process_group: 101,
                foreground_group: Some(101),
                terminal: Some("ttys002".into()),
                args: "-zsh".into(),
            },
        ];
        let parents = processes
            .iter()
            .map(|process| (process.pid, process.parent_pid))
            .collect();
        let sshd_pids = HashSet::from([50]);
        let connection = SshConnection {
            client_address: "100.64.0.1".into(),
            client_port: 50000,
            server_address: "100.64.0.2".into(),
            server_port: 22,
        };
        let sshd_connections = HashMap::from([(50, connection)]);
        let single_tty_parents = processes[..3]
            .iter()
            .map(|process| (process.pid, process.parent_pid))
            .collect();
        assert!(
            unambiguous_ssh_connections(
                &processes[..3],
                &single_tty_parents,
                &sshd_pids,
                &sshd_connections
            )
            .contains_key(&200)
        );
        assert!(
            unambiguous_ssh_connections(&processes, &parents, &sshd_pids, &sshd_connections)
                .is_empty()
        );
    }

    #[test]
    fn nested_runner_pty_keeps_its_outer_ssh_connection() {
        let processes = [
            Process {
                uid: 502,
                pid: 50,
                parent_pid: 1,
                process_group: 50,
                foreground_group: None,
                terminal: None,
                args: "sshd: user".into(),
            },
            Process {
                uid: 502,
                pid: 100,
                parent_pid: 50,
                process_group: 100,
                foreground_group: Some(200),
                terminal: Some("ttys001".into()),
                args: "-zsh".into(),
            },
            Process {
                uid: 502,
                pid: 200,
                parent_pid: 100,
                process_group: 200,
                foreground_group: Some(200),
                terminal: Some("ttys001".into()),
                args: "tmux-agent run -- codex".into(),
            },
            Process {
                uid: 502,
                pid: 201,
                parent_pid: 200,
                process_group: 201,
                foreground_group: Some(201),
                terminal: Some("ttys099".into()),
                args: "codex".into(),
            },
        ];
        let parents = processes
            .iter()
            .map(|process| (process.pid, process.parent_pid))
            .collect();
        let sshd_pids = HashSet::from([50]);
        let connection = SshConnection {
            client_address: "100.64.0.1".into(),
            client_port: 50000,
            server_address: "100.64.0.2".into(),
            server_port: 22,
        };
        let sshd_connections = HashMap::from([(50, connection.clone())]);
        let resolved =
            unambiguous_ssh_connections(&processes, &parents, &sshd_pids, &sshd_connections);
        assert_eq!(resolved.get(&200), Some(&connection));
        assert_eq!(resolved.get(&201), Some(&connection));
    }
}
