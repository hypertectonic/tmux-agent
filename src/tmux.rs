use crate::config::Config;
use crate::model::{
    AgentRecord, SshConnection, SshTransport, TmuxTarget, terminal_safe,
    trim_braille_activity_prefix,
};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SEPARATOR: char = '\u{1f}';
const ESCAPED_SEPARATOR: &str = r"\037";
const UI_SELECTION_OPTION: &str = "@tmux_agent_selection";
// CSI 34~ is an otherwise unused F17 key that wakes every persistent UI after
// an explicit numeric selection without introducing polling or focus tracking.
const UI_SELECTION_WAKE_HEX: [&str; 5] = ["1b", "5b", "33", "34", "7e"];
const PANE_HOST_COLORS: [&str; 8] = [
    "#89b4fa", "#cba6f7", "#fab387", "#f9e2af", "#a6e3a1", "#f38ba8", "#74c7ec", "#94e2d5",
];

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
    pub context_host: Option<String>,
    pub context_host_color: Option<String>,
}

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
    pub ssh_transports: Vec<SshTransport>,
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
}

#[derive(Debug)]
struct FocusTargetMissing {
    alias: String,
    title: String,
}

#[derive(Debug, PartialEq, Eq)]
enum PaneHostChange {
    Set {
        pane_id: String,
        host: String,
        color: String,
    },
    Clear {
        pane_id: String,
    },
}

impl fmt::Display for FocusTargetMissing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "no unique local SSH pane for {} with title {:?}",
            self.alias, self.title
        )
    }
}

impl std::error::Error for FocusTargetMissing {}

pub fn is_focus_target_missing(error: &anyhow::Error) -> bool {
    error.downcast_ref::<FocusTargetMissing>().is_some()
}

impl Tmux {
    pub fn new(config: &Config) -> Self {
        Self {
            args: config.tmux_args.clone(),
            host_aliases: pane_host_aliases(config),
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
            "#{@tmux_agent_host}",
            "#{@tmux_agent_host_color}",
        ]
        .join(&sep);
        let Some(output) = self.run_optional(&["list-panes", "-a", "-F", &format])? else {
            return Ok(Vec::new());
        };
        output.lines().map(parse_pane).collect()
    }

    pub fn process_snapshot(&self, panes: &[Pane]) -> Result<ProcessSnapshot> {
        let output = Command::new("ps")
            .args([
                "-axww",
                "-o",
                "uid=,pid=,ppid=,pgid=,tpgid=,tty=,etime=,args=",
            ])
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
        let processes = process_output
            .lines()
            .filter_map(parse_process)
            .filter(|process| process.uid == unsafe { libc::geteuid() })
            .collect::<Vec<_>>();
        let mut pane_descriptions = HashMap::new();
        let mut pane_groups = HashMap::new();
        let mut pane_pids = HashMap::new();
        let mut tmux_process_groups = HashSet::new();
        for pane in panes {
            if let Some((process_group, description, pids)) =
                foreground_job(pane.pane_pid, &processes)
            {
                tmux_process_groups.insert(process_group);
                pane_groups.insert(pane.pane_id.clone(), process_group);
                pane_pids.insert(pane.pane_id.clone(), pids);
                pane_descriptions.insert(pane.pane_id.clone(), description);
            }
        }
        let socket_pids = processes
            .iter()
            .filter(|process| {
                is_ssh_program(&process.args) || is_sshd_session_program(&process.args)
            })
            .map(|process| process.pid)
            .collect::<Vec<_>>();
        let tcp_connections = self.process_tcp_connections(&socket_pids);
        let parents = processes
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
            unambiguous_ssh_connections(&processes, &parents, &sshd_pids, &sshd_connections);
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
        Ok(ProcessSnapshot {
            panes: pane_descriptions,
            pane_groups,
            pane_pids,
            process_args: processes
                .iter()
                .map(|process| (process.pid, process.args.clone()))
                .collect(),
            process_started_at_ms,
            live_pids: processes.iter().map(|process| process.pid).collect(),
            terminals: foreground_terminal_jobs(
                &processes,
                &tmux_process_groups,
                &tmux_pane_terminals(panes, &processes),
            ),
            parent_pids: parents,
            ssh_connections,
            ssh_transports,
        })
    }

    fn pane_processes(&self, panes: &[Pane]) -> Result<HashMap<String, String>> {
        let output = Command::new("ps")
            .args([
                "-axww",
                "-o",
                "uid=,pid=,ppid=,pgid=,tpgid=,tty=,etime=,args=",
            ])
            .output()
            .context("run ps for pane host discovery")?;
        if !output.status.success() {
            bail!("ps failed with {}", output.status);
        }
        let processes = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_process)
            .filter(|process| process.uid == unsafe { libc::geteuid() })
            .collect::<Vec<_>>();
        Ok(panes
            .iter()
            .filter_map(|pane| {
                foreground_job(pane.pane_pid, &processes)
                    .map(|(_, description, _)| (pane.pane_id.clone(), description))
            })
            .collect())
    }

    pub fn capture_visible(&self, pane_id: &str) -> Result<String> {
        self.run(&visible_capture_args(pane_id))
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

    pub fn focus_agent(&self, record: &AgentRecord) -> Result<()> {
        if let Some(alias) = &record.remote_alias {
            if let Some(target) = &record.focus_target {
                return self.focus_location_with_host(
                    &target.session_name,
                    &target.window_id,
                    &target.pane_id,
                    &record.host,
                );
            }
            if record.is_tmux() {
                if let Some(mirror) = self.find_mirror(alias, &record.session_name)? {
                    return self.focus_location_with_host(
                        &mirror.session_name,
                        &mirror.window_id,
                        &mirror.pane_id,
                        &record.host,
                    );
                }
            } else if let Some(pane) = self.find_transport_pane(alias, &record.title)? {
                return self.focus_location_with_host(
                    &pane.session_name,
                    &pane.window_id,
                    &pane.pane_id,
                    &record.host,
                );
            }
            return Err(FocusTargetMissing {
                alias: alias.clone(),
                title: record.title.clone(),
            }
            .into());
        }
        if !record.is_tmux() {
            bail!(
                "{} is an ordinary terminal session and cannot be focused through tmux",
                record.location()
            );
        }
        self.focus_location_with_host(
            &record.session_name,
            &record.window_id,
            &record.pane_id,
            &record.host,
        )
    }

    pub fn find_mirror(&self, remote_alias: &str, remote_session: &str) -> Result<Option<Pane>> {
        let panes = self.list_panes()?;
        Ok(find_mirror_pane(&panes, remote_alias, remote_session)?.cloned())
    }

    fn find_transport_pane(&self, remote_alias: &str, title: &str) -> Result<Option<Pane>> {
        let panes = self.list_panes()?;
        let processes = self.process_snapshot(&panes)?;
        Ok(find_transport_pane(&panes, &processes.panes, remote_alias, title)?.cloned())
    }

    pub fn set_ui_marker(&self, pane_id: &str, enabled: bool) -> Result<()> {
        let value = if enabled { "1" } else { "" };
        self.status(&["set-option", "-p", "-t", pane_id, "@tmux_agent_ui", value])
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

    pub fn reconcile_pane_hosts(
        &self,
        overrides: &HashMap<String, String>,
        local_host: &str,
    ) -> Result<()> {
        let panes = self.list_panes()?;
        if panes.is_empty() {
            return Ok(());
        }
        let pane_processes = self.pane_processes(&panes)?;
        let desired = pane_host_presentations(
            &panes,
            &pane_processes,
            overrides,
            local_host,
            &self.host_aliases,
        );
        for change in pane_host_changes(&panes, &desired) {
            match change {
                PaneHostChange::Set {
                    pane_id,
                    host,
                    color,
                } => {
                    self.set_pane_host(&pane_id, &host, &color)?;
                }
                PaneHostChange::Clear { pane_id } => {
                    self.status(&["set-option", "-p", "-u", "-t", &pane_id, "@tmux_agent_host"])?;
                    self.status(&[
                        "set-option",
                        "-p",
                        "-u",
                        "-t",
                        &pane_id,
                        "@tmux_agent_host_color",
                    ])?;
                }
            }
        }
        Ok(())
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

    fn focus_location_with_host(
        &self,
        session: &str,
        window: &str,
        pane: &str,
        host: &str,
    ) -> Result<()> {
        let host = display_host(host);
        let color = pane_host_color(&host);
        if let Err(error) = self.set_pane_host(pane, &host, color) {
            eprintln!("tmux-agent: set pane host context for {pane}: {error:#}");
        }
        self.focus_location(session, window, pane)
    }

    fn set_pane_host(&self, pane_id: &str, host: &str, color: &str) -> Result<()> {
        self.status(&["set-option", "-p", "-t", pane_id, "@tmux_agent_host", host])?;
        self.status(&[
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@tmux_agent_host_color",
            color,
        ])
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

fn pane_host_aliases(config: &Config) -> HashMap<String, String> {
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
        remote_host: resolved_host_alias(aliases, remote_host),
        remote_host_explicit: pane.mirror_host.is_some(),
        remote_session: pane.mirror_session.clone(),
        title: normalize_transport_title(&pane.title),
        label: pane.label.clone(),
        target: TmuxTarget {
            session_name: pane.session_name.clone(),
            window_id: pane.window_id.clone(),
            window_index: pane.window_index,
            pane_id: pane.pane_id.clone(),
            pane_index: pane.pane_index,
        },
    }
}

fn pane_host_presentations(
    panes: &[Pane],
    pane_processes: &HashMap<String, String>,
    overrides: &HashMap<String, String>,
    local_host: &str,
    aliases: &HashMap<String, String>,
) -> HashMap<String, (String, String)> {
    panes
        .iter()
        .map(|pane| {
            let observed = overrides
                .get(&pane.pane_id)
                .map(String::as_str)
                .or(pane.mirror_host.as_deref())
                .or_else(|| {
                    pane_processes
                        .get(&pane.pane_id)
                        .and_then(|processes| ssh_destination(processes))
                })
                .or_else(|| mosh_destination_from_title(&pane.title))
                .unwrap_or(local_host);
            let alias_key = observed.trim_end_matches('.').to_ascii_lowercase();
            let host = display_host(
                aliases
                    .get(&alias_key)
                    .map(String::as_str)
                    .unwrap_or(observed),
            );
            let color = pane_host_color(&host).to_string();
            (pane.pane_id.clone(), (host, color))
        })
        .collect()
}

fn display_host(host: &str) -> String {
    terminal_safe(host).to_uppercase()
}

fn pane_host_color(host: &str) -> &'static str {
    let hash = host
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    PANE_HOST_COLORS[hash as usize % PANE_HOST_COLORS.len()]
}

fn pane_host_changes(
    panes: &[Pane],
    desired: &HashMap<String, (String, String)>,
) -> Vec<PaneHostChange> {
    panes
        .iter()
        .filter_map(|pane| {
            match (
                pane.context_host.as_deref(),
                pane.context_host_color.as_deref(),
                desired.get(&pane.pane_id),
            ) {
                (Some(current_host), Some(current_color), Some((host, color)))
                    if current_host == host && current_color == color =>
                {
                    None
                }
                (_, _, Some((host, color))) => Some(PaneHostChange::Set {
                    pane_id: pane.pane_id.clone(),
                    host: host.clone(),
                    color: color.clone(),
                }),
                (Some(_), _, None) | (_, Some(_), None) => Some(PaneHostChange::Clear {
                    pane_id: pane.pane_id.clone(),
                }),
                (None, None, None) => None,
            }
        })
        .collect()
}

fn normalized_socket_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

fn visible_capture_args(pane_id: &str) -> [&str; 4] {
    ["capture-pane", "-p", "-t", pane_id]
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
    if fields.len() != 21 {
        fields = line.split(ESCAPED_SEPARATOR).collect::<Vec<_>>();
    }
    if fields.len() != 21 {
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
        context_host: nonempty(fields[19]),
        context_host_color: nonempty(fields[20]),
    })
}

fn pane_is_visible(window_active: &str, session_attached: &str) -> bool {
    window_active == "1" && session_attached != "0"
}

fn parse_process(line: &str) -> Option<Process> {
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
    let terminal = fields
        .next()
        .filter(|value| !matches!(*value, "??" | "?"))
        .map(str::to_string);
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
    stderr.contains("no server running")
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
            ssh_destination(processes).is_some_and(|host| host == remote_alias)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [pane] => Ok(Some(*pane)),
        _ => bail!(
            "multiple local SSH panes match {remote_alias} with title {title:?}: {}",
            matches
                .iter()
                .map(|pane| pane.pane_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn normalize_transport_title(value: &str) -> String {
    trim_braille_activity_prefix(value).trim_end().to_string()
}

fn mosh_destination_from_title(title: &str) -> Option<&str> {
    let destination = title.strip_prefix("[mosh] ")?.split(':').next()?.trim();
    (!destination.is_empty()).then_some(destination)
}

fn ssh_destination(processes: &str) -> Option<&str> {
    processes
        .lines()
        .next()
        .and_then(ssh_destination_for_command)
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
    use tempfile::tempdir;

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
            context_host: None,
            context_host_color: None,
        }
    }

    #[test]
    fn parses_ps_rows_with_full_arguments() {
        let process =
            parse_process("  502 123 10 123 123 ttys003 01:02 /usr/bin/codex --model smart")
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
    fn every_pane_in_an_attached_active_window_is_visible() {
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
            "",
            "",
            "remote development",
            "BUILD-HOST",
            "#a6e3a1",
        ]
        .join(&separator);
        let pane = parse_pane(&line).unwrap();
        assert!(pane.visible);
        assert!(!pane.dead);
        assert_eq!(pane.label.as_deref(), Some("remote development"));
        assert_eq!(pane.context_host.as_deref(), Some("BUILD-HOST"));
        assert_eq!(pane.context_host_color.as_deref(), Some("#a6e3a1"));
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
            "1", "1", "", "", "", "", "", "",
        ]
        .join(ESCAPED_SEPARATOR);

        let pane = parse_pane(&line).unwrap();

        assert_eq!(pane.pane_id, "%1");
        assert_eq!(pane.pane_pid, 123);
        assert!(pane.visible);
    }

    #[test]
    fn pane_host_reconciliation_sets_changes_and_clears_stale_context() {
        let mut missing = pane("%1", "main", "one");
        let mut current = pane("%2", "main", "two");
        current.context_host = Some("BUILD-HOST".into());
        current.context_host_color = Some("#a6e3a1".into());
        let mut stale = pane("%3", "main", "three");
        stale.context_host = Some("LINUX-MACHINE".into());
        stale.context_host_color = Some("#f38ba8".into());
        let desired = HashMap::from([
            (
                "%1".to_string(),
                ("LOCAL-MAC".to_string(), "#fab387".to_string()),
            ),
            (
                "%2".to_string(),
                ("BUILD-HOST".to_string(), "#a6e3a1".to_string()),
            ),
        ]);

        let changes = pane_host_changes(&[missing.clone(), current, stale], &desired);

        assert_eq!(
            changes,
            vec![
                PaneHostChange::Set {
                    pane_id: "%1".into(),
                    host: "LOCAL-MAC".into(),
                    color: "#fab387".into(),
                },
                PaneHostChange::Clear {
                    pane_id: "%3".into(),
                },
            ]
        );

        missing.context_host = Some("OLD-HOST".into());
        missing.context_host_color = Some("#fab387".into());
        assert_eq!(
            pane_host_changes(&[missing], &desired),
            vec![PaneHostChange::Set {
                pane_id: "%1".into(),
                host: "LOCAL-MAC".into(),
                color: "#fab387".into(),
            }]
        );
    }

    #[test]
    fn pane_host_presentations_cover_local_ssh_mosh_and_explicit_agent_panes() {
        let mut local = pane("%1", "main", "shell");
        local.current_command = "zsh".into();
        let ssh = pane("%2", "main", "remote shell");
        let mosh = pane("%3", "main", "[mosh] build-host:TUI");
        let explicit = pane("%4", "main", "provider");
        let unknown_ssh = pane("%5", "main", "other remote");
        let panes = vec![local, ssh, mosh, explicit, unknown_ssh];
        let processes = HashMap::from([
            ("%1".to_string(), "/bin/zsh".to_string()),
            (
                "%2".to_string(),
                "/usr/bin/ssh -tt remote-mac.example.ts.net".to_string(),
            ),
            ("%5".to_string(), "/usr/bin/ssh linux-machine".to_string()),
        ]);
        let overrides = HashMap::from([("%4".to_string(), "build-host".to_string())]);
        let aliases = HashMap::from([
            ("build-host".to_string(), "build-host".to_string()),
            (
                "remote-mac.example.ts.net".to_string(),
                "build-host".to_string(),
            ),
        ]);

        let presentations =
            pane_host_presentations(&panes, &processes, &overrides, "local-mac", &aliases);

        assert_eq!(
            presentations.get("%1"),
            Some(&("LOCAL-MAC".into(), "#fab387".into()))
        );
        assert_eq!(
            presentations.get("%2"),
            Some(&("BUILD-HOST".into(), "#a6e3a1".into()))
        );
        assert_eq!(
            presentations.get("%3"),
            Some(&("BUILD-HOST".into(), "#a6e3a1".into()))
        );
        assert_eq!(
            presentations.get("%4"),
            Some(&("BUILD-HOST".into(), "#a6e3a1".into()))
        );
        assert_eq!(
            presentations.get("%5"),
            Some(&("LINUX-MACHINE".into(), "#f38ba8".into()))
        );
    }

    #[test]
    fn pane_host_colors_are_stable_and_distinct_for_known_machines() {
        assert_eq!(pane_host_color("LOCAL-MAC"), "#fab387");
        assert_eq!(pane_host_color("BUILD-HOST"), "#a6e3a1");
        assert_eq!(pane_host_color("LINUX-MACHINE"), "#f38ba8");
    }

    #[test]
    fn parses_dead_panes() {
        let separator = SEPARATOR.to_string();
        let line = [
            "%1", "123", "$1", "main", "@1", "1", "work", "0", "codex", "title", "/tmp", "1", "0",
            "1", "1", "", "", "", "", "", "",
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
            "",
            "",
        ]
        .join(&separator);

        let preferred = parse_pane(&preferred).unwrap();

        assert_eq!(preferred.mirror_host.as_deref(), Some("remote-mac"));
        assert_eq!(preferred.mirror_session.as_deref(), Some("project"));
    }

    #[test]
    fn recognizes_missing_tmux_server_errors() {
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
        let args = visible_capture_args("%42");
        assert_eq!(args, ["capture-pane", "-p", "-t", "%42"]);
        assert!(!args.contains(&"-S"));
        assert!(!args.contains(&"-M"));
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
    fn transport_titles_ignore_provider_braille_spinners() {
        assert_eq!(normalize_transport_title("⠂ project"), "project");
        assert_eq!(normalize_transport_title("⠸ project"), "project");
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
    fn ssh_destination_skips_options_and_user_prefix() {
        assert_eq!(
            ssh_destination("/usr/bin/ssh -tt -o BatchMode=yes -p 22 agent@host"),
            Some("host")
        );
        assert_eq!(ssh_destination("ssh -vJ jump remote"), Some("remote"));
        assert_eq!(ssh_destination("ssh -vJjump remote"), Some("remote"));
        assert_eq!(ssh_destination("mosh remote-mac"), None);
        assert_eq!(ssh_destination("ssh -p 22"), None);
    }

    #[test]
    fn ssh_destination_requires_the_foreground_group_leader_to_be_ssh() {
        assert_eq!(ssh_destination("git fetch\nssh remote-mac"), None);
        assert_eq!(
            ssh_destination("ssh remote-mac\nhelper process"),
            Some("remote-mac")
        );
    }

    #[test]
    fn only_explicit_focus_misses_are_classified_as_missing() {
        let missing = anyhow::Error::new(FocusTargetMissing {
            alias: "remote-mac".into(),
            title: "project".into(),
        });
        assert!(is_focus_target_missing(&missing));
        assert!(!is_focus_target_missing(&anyhow::anyhow!("tmux failed")));
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
