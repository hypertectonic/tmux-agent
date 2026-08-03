use crate::config::{Config, RuntimePaths, default_config_path};
use crate::ipc;
use crate::model::{
    APPLICATION_VERSION, CAPABILITY_SUBAGENT_VIEW, PROTOCOL_VERSION, Snapshot, terminal_safe,
};
use crate::tmux::Tmux;
use anyhow::Result;
use serde::Serialize;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    status: CheckStatus,
    message: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    application_version: String,
    protocol: u32,
    operating_system: String,
    architecture: String,
    config_path: String,
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    fn check(&mut self, name: impl Into<String>, status: CheckStatus, message: impl Into<String>) {
        let message = private_text(&message.into());
        self.checks.push(DoctorCheck {
            name: name.into(),
            status,
            message: terminal_safe(&message),
        });
    }

    fn has_errors(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == CheckStatus::Error)
    }
}

pub async fn run(explicit_config: Option<&Path>, json: bool) -> Result<()> {
    let config_path = explicit_config
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    let mut report = DoctorReport {
        application_version: APPLICATION_VERSION.to_string(),
        protocol: PROTOCOL_VERSION,
        operating_system: env::consts::OS.to_string(),
        architecture: env::consts::ARCH.to_string(),
        config_path: private_path(&config_path),
        checks: Vec::new(),
    };

    report.check(
        "application",
        CheckStatus::Ok,
        format!(
            "tmux-agent {} using protocol {}",
            APPLICATION_VERSION, PROTOCOL_VERSION
        ),
    );
    match supported_target(env::consts::OS, env::consts::ARCH) {
        Some(target) => report.check("platform", CheckStatus::Ok, target),
        None => report.check(
            "platform",
            CheckStatus::Error,
            format!(
                "unsupported platform {}/{}",
                env::consts::OS,
                env::consts::ARCH
            ),
        ),
    }

    for command in ["tmux", "ps", "lsof", "ssh"] {
        let status = if command_exists(command) {
            CheckStatus::Ok
        } else if matches!(command, "tmux" | "ps") {
            CheckStatus::Error
        } else {
            CheckStatus::Warning
        };
        let message = if status == CheckStatus::Ok {
            "available"
        } else {
            "not found on PATH"
        };
        report.check(format!("command:{command}"), status, message);
    }

    let (config, loaded_config_path) = match Config::load(explicit_config) {
        Ok(loaded) => {
            let state = if loaded.1.exists() {
                "parsed successfully"
            } else {
                "not present; using defaults"
            };
            report.check("config", CheckStatus::Ok, state);
            loaded
        }
        Err(error) => {
            report.check(
                "config",
                CheckStatus::Error,
                format!("could not parse or validate config: {error:#}"),
            );
            return finish(report, json);
        }
    };

    let tmux = Tmux::new(&config);
    match command_version("tmux", &["-V"]) {
        Some(version) if tmux_version_supported(&version) => {
            report.check("tmux-version", CheckStatus::Ok, version)
        }
        Some(version) => report.check(
            "tmux-version",
            CheckStatus::Error,
            format!("{version}; tmux 3.2 or newer is required"),
        ),
        None => report.check(
            "tmux-version",
            CheckStatus::Error,
            "could not read tmux version",
        ),
    }
    match tmux.server_key() {
        Ok(Some(server)) => {
            report.check(
                "tmux-server",
                CheckStatus::Ok,
                format!("selected {}", private_text(&server)),
            );
        }
        Ok(None) => {
            report.check(
                "tmux-server",
                CheckStatus::Warning,
                "no tmux server is currently available",
            );
        }
        Err(error) => {
            report.check(
                "tmux-server",
                CheckStatus::Warning,
                format!("could not inspect the selected server: {error:#}"),
            );
        }
    }

    let paths = RuntimePaths::discover(&tmux.runtime_key())?;
    match paths.ensure_dirs() {
        Ok(()) => {
            check_private_directory(&mut report, "runtime-directory", paths.socket.parent());
            check_private_directory(&mut report, "state-directory", paths.state.parent());
        }
        Err(error) => report.check(
            "runtime-directories",
            CheckStatus::Error,
            format!("could not prepare private directories: {error:#}"),
        ),
    }

    let snapshot = match ipc::snapshot(&paths.socket, false).await {
        Ok(snapshot) => {
            let version = snapshot.application_version.as_deref().unwrap_or("unknown");
            let status = daemon_compatibility(&snapshot);
            let message = if status == CheckStatus::Ok {
                format!(
                    "socket available; daemon version {version}, protocol {}",
                    snapshot.protocol
                )
            } else {
                format!(
                    "socket available; daemon version {version}, protocol {}; expected version {}, protocol {}; run tmux-agent daemon restart",
                    snapshot.protocol, APPLICATION_VERSION, PROTOCOL_VERSION
                )
            };
            report.check("daemon", status, message);
            Some(snapshot)
        }
        Err(_) => {
            report.check(
                "daemon",
                CheckStatus::Warning,
                format!("not reachable at {}", private_path(&paths.socket)),
            );
            None
        }
    };

    check_plugin_version(&mut report);
    if let Some(snapshot) = snapshot.as_ref() {
        check_peer_health(&mut report, snapshot);
    }
    check_structured_machines(&mut report, &config, &loaded_config_path).await;
    for remote in &config.remotes {
        report.check(
            format!("remote:{}", remote.name),
            CheckStatus::Warning,
            "raw collector configured; version and capability checks are unavailable",
        );
    }

    finish(report, json)
}

fn finish(report: DoctorReport, json: bool) -> Result<()> {
    let has_errors = report.has_errors();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "tmux-agent {}  protocol {}  {}/{}",
            report.application_version,
            report.protocol,
            report.operating_system,
            report.architecture
        );
        println!("config {}", report.config_path);
        for check in &report.checks {
            let icon = match check.status {
                CheckStatus::Ok => "ok",
                CheckStatus::Warning => "warn",
                CheckStatus::Error => "error",
            };
            println!("{icon:>5}  {:<24} {}", check.name, check.message);
        }
    }
    if has_errors {
        std::process::exit(1);
    }
    Ok(())
}

fn check_plugin_version(report: &mut DoctorReport) {
    let Some(root) = env::var_os("TMUX_AGENT_ROOT").map(PathBuf::from) else {
        report.check(
            "plugin-version",
            CheckStatus::Warning,
            "not running through the plugin launcher",
        );
        return;
    };
    let version_path = root.join("VERSION");
    match fs::read_to_string(&version_path) {
        Ok(value) if value.trim() == APPLICATION_VERSION => report.check(
            "plugin-version",
            CheckStatus::Ok,
            format!("plugin and binary agree on {}", APPLICATION_VERSION),
        ),
        Ok(value) => report.check(
            "plugin-version",
            CheckStatus::Error,
            format!(
                "plugin expects {} but binary reports {}; run tmux-agent plugin update",
                value.trim(),
                APPLICATION_VERSION
            ),
        ),
        Err(error) => report.check(
            "plugin-version",
            CheckStatus::Error,
            format!("could not read {}: {error}", private_path(&version_path)),
        ),
    }
}

fn check_peer_health(report: &mut DoctorReport, snapshot: &Snapshot) {
    for peer in &snapshot.peers {
        let name = format!("peer:{}", peer.name);
        if !peer.connected {
            report.check(
                name,
                CheckStatus::Warning,
                peer.last_error
                    .as_deref()
                    .unwrap_or("remote collector is disconnected"),
            );
            continue;
        }
        if peer.protocol != PROTOCOL_VERSION {
            report.check(
                name,
                CheckStatus::Error,
                format!(
                    "protocol {} is incompatible with local protocol {}",
                    peer.protocol, PROTOCOL_VERSION
                ),
            );
            continue;
        }
        let version = peer.application_version.as_deref().unwrap_or("unknown");
        let capability = if peer
            .capabilities
            .iter()
            .any(|value| value == CAPABILITY_SUBAGENT_VIEW)
        {
            "subagent view available"
        } else {
            "subagent view unavailable; update the remote binary"
        };
        report.check(
            name,
            CheckStatus::Ok,
            format!(
                "version {version}, protocol {}; {capability}",
                peer.protocol
            ),
        );
    }
    for alias in snapshot
        .agents
        .iter()
        .filter_map(|agent| agent.remote_alias.as_deref())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let remote_agents = snapshot
            .agents
            .iter()
            .filter(|agent| agent.remote_alias.as_deref() == Some(alias))
            .collect::<Vec<_>>();
        let transport_records = remote_agents
            .iter()
            .filter(|agent| agent.ssh_connection.is_some())
            .collect::<Vec<_>>();
        if transport_records.is_empty() {
            report.check(
                format!("focus:{alias}"),
                CheckStatus::Warning,
                "no remote record currently exposes an SSH connection tuple",
            );
        } else if transport_records
            .iter()
            .any(|agent| agent.focus_target.is_some())
        {
            report.check(
                format!("focus:{alias}"),
                CheckStatus::Ok,
                "a remote record resolves to a local SSH pane",
            );
        } else {
            report.check(
                format!("focus:{alias}"),
                CheckStatus::Warning,
                "remote records do not resolve to a unique local SSH pane",
            );
        }
    }
}

async fn check_structured_machines(report: &mut DoctorReport, config: &Config, config_path: &Path) {
    for machine in &config.machines {
        let command = machine.diagnostic_command();
        let Some((program, arguments)) = command.split_first() else {
            continue;
        };
        let output = diagnostic_output(program, arguments, Duration::from_secs(10)).await;
        let name = format!("remote:{}", machine.name);
        let output = match output {
            Ok(Ok(output)) if output.status.success() => output,
            Ok(Ok(output)) => {
                report.check(
                    name,
                    CheckStatus::Warning,
                    format!("SSH diagnostic exited with {}", output.status),
                );
                continue;
            }
            Ok(Err(error)) => {
                report.check(
                    name,
                    CheckStatus::Warning,
                    format!("could not start SSH diagnostic: {error}"),
                );
                continue;
            }
            Err(_) => {
                report.check(
                    name,
                    CheckStatus::Warning,
                    "SSH diagnostic timed out after 10 seconds",
                );
                continue;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut lines = stdout.lines();
        let remote_version = lines.next().unwrap_or_default();
        let snapshot_json = lines.collect::<Vec<_>>().join("\n");
        let snapshot = serde_json::from_str::<Snapshot>(&snapshot_json);
        match snapshot {
            Ok(snapshot) if snapshot.protocol == PROTOCOL_VERSION => {
                let capability = if snapshot
                    .capabilities
                    .iter()
                    .any(|value| value == CAPABILITY_SUBAGENT_VIEW)
                {
                    "subagent view available"
                } else {
                    "subagent view unavailable"
                };
                report.check(
                    name,
                    CheckStatus::Ok,
                    format!(
                        "{remote_version}; protocol {}; {capability}",
                        snapshot.protocol
                    ),
                );
            }
            Ok(snapshot) => report.check(
                name,
                CheckStatus::Error,
                format!(
                    "{remote_version}; protocol {} is incompatible with local protocol {}",
                    snapshot.protocol, PROTOCOL_VERSION
                ),
            ),
            Err(error) => report.check(
                name,
                CheckStatus::Warning,
                format!(
                    "remote binary did not return a valid diagnostic snapshot: {error}; config {}",
                    private_path(config_path)
                ),
            ),
        }
    }
}

async fn diagnostic_output(
    program: &str,
    arguments: &[String],
    limit: Duration,
) -> std::result::Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed> {
    let mut command = tokio::process::Command::new(program);
    command.args(arguments).kill_on_drop(true);
    tokio::time::timeout(limit, command.output()).await
}

fn check_private_directory(report: &mut DoctorReport, name: &str, path: Option<&Path>) {
    let Some(path) = path else {
        report.check(name, CheckStatus::Error, "path has no parent directory");
        return;
    };
    match fs::metadata(path) {
        Ok(metadata) => {
            #[cfg(unix)]
            {
                let mode = metadata.permissions().mode() & 0o777;
                if mode & 0o077 == 0 {
                    report.check(
                        name,
                        CheckStatus::Ok,
                        format!("{} has private permissions {mode:o}", private_path(path)),
                    );
                } else {
                    report.check(
                        name,
                        CheckStatus::Error,
                        format!(
                            "{} permissions {mode:o} allow group or other access",
                            private_path(path)
                        ),
                    );
                }
            }
            #[cfg(not(unix))]
            report.check(name, CheckStatus::Ok, private_path(path));
        }
        Err(error) => report.check(
            name,
            CheckStatus::Error,
            format!("cannot inspect {}: {error}", private_path(path)),
        ),
    }
}

fn command_exists(command: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|directory| directory.join(command).is_file())
}

fn command_version(command: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(command).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn tmux_version_supported(version: &str) -> bool {
    let version = version
        .trim()
        .strip_prefix("tmux ")
        .unwrap_or(version.trim());
    let version = version.strip_prefix("next-").unwrap_or(version);
    let numeric = version
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>();
    let mut parts = numeric.split('.');
    let Some(major) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let Some(minor) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    (major, minor) >= (3, 2)
}

fn daemon_compatibility(snapshot: &Snapshot) -> CheckStatus {
    if snapshot.protocol == PROTOCOL_VERSION
        && snapshot.application_version.as_deref() == Some(APPLICATION_VERSION)
    {
        CheckStatus::Ok
    } else {
        CheckStatus::Error
    }
}

fn supported_target(operating_system: &str, architecture: &str) -> Option<&'static str> {
    match (operating_system, architecture) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

fn private_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if value == home {
            return "~".to_string();
        }
        if let Some(relative) = value.strip_prefix(&format!("{home}/")) {
            return format!("~/{relative}");
        }
    }
    terminal_safe(&value)
}

fn private_text(value: &str) -> String {
    dirs::home_dir()
        .map(|home| value.replace(&home.to_string_lossy().to_string(), "~"))
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn supported_platforms_map_to_release_targets() {
        assert_eq!(
            supported_target("macos", "aarch64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            supported_target("linux", "x86_64"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(supported_target("windows", "x86_64"), None);
    }

    #[test]
    fn report_error_state_is_machine_readable() {
        let mut report = DoctorReport {
            application_version: "0.1.0".into(),
            protocol: 1,
            operating_system: "linux".into(),
            architecture: "x86_64".into(),
            config_path: "~/.config/tmux-agent/config.toml".into(),
            checks: Vec::new(),
        };
        report.check("config", CheckStatus::Error, "invalid");
        let encoded = serde_json::to_value(&report).unwrap();

        assert!(report.has_errors());
        assert_eq!(encoded["checks"][0]["status"], "error");
    }

    #[test]
    fn tmux_minimum_version_is_enforced() {
        assert!(!tmux_version_supported("tmux 3.1c"));
        assert!(tmux_version_supported("tmux 3.2"));
        assert!(tmux_version_supported("tmux 3.2a"));
        assert!(tmux_version_supported("tmux 3.5"));
        assert!(tmux_version_supported("tmux next-3.6"));
        assert!(!tmux_version_supported("tmux unknown"));
    }

    #[test]
    fn daemon_version_and_protocol_must_both_match() {
        let mut snapshot = Snapshot {
            protocol: PROTOCOL_VERSION,
            application_version: Some(APPLICATION_VERSION.to_string()),
            ..Snapshot::default()
        };
        assert_eq!(daemon_compatibility(&snapshot), CheckStatus::Ok);

        snapshot.application_version = Some("0.0.9".to_string());
        assert_eq!(daemon_compatibility(&snapshot), CheckStatus::Error);

        snapshot.application_version = Some(APPLICATION_VERSION.to_string());
        snapshot.protocol += 1;
        assert_eq!(daemon_compatibility(&snapshot), CheckStatus::Error);
    }

    #[tokio::test]
    async fn timed_out_diagnostic_process_is_killed() {
        let directory = tempdir().unwrap();
        let pid_path = directory.path().join("diagnostic.pid");
        let arguments = vec![
            "-c".to_string(),
            format!(
                "echo $$ > '{}'; exec sleep 30",
                pid_path.display().to_string().replace('\'', "'\"'\"'")
            ),
        ];

        let result = diagnostic_output("/bin/sh", &arguments, Duration::from_millis(200)).await;
        assert!(result.is_err());

        let pid = fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return;
                }
                panic!("could not inspect timed-out diagnostic process {pid}: {error}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed-out diagnostic process {pid} is still running");
    }
}
