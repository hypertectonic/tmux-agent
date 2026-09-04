use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host_name: Option<String>,
    pub server_name: Option<String>,
    pub scan_interval_ms: Option<u64>,
    pub tmux_args: Vec<String>,
    #[serde(rename = "machine")]
    pub machines: Vec<MachineConfig>,
    #[serde(rename = "remote")]
    pub remotes: Vec<RemoteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MachineConfig {
    pub name: String,
    pub host: String,
    pub ssh_user: String,
    pub binary: String,
    #[serde(default = "enabled_by_default")]
    pub auto_connect: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteConfig {
    pub name: String,
    pub command: Vec<String>,
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = explicit
            .map(Path::to_path_buf)
            .unwrap_or_else(default_config_path);
        if !path.exists() {
            return Ok((Self::default(), path));
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("read config {}", path.display()))?;
        let config: Self =
            toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
        config.validate()?;
        Ok((config, path))
    }

    fn validate(&self) -> Result<()> {
        let mut names = HashSet::new();
        for machine in &self.machines {
            validate_name(&machine.name)?;
            if !names.insert(machine.name.as_str()) {
                bail!("duplicate machine or remote name {}", machine.name);
            }
            validate_host(&machine.host, &machine.name)?;
            validate_ssh_user(&machine.ssh_user, &machine.name)?;
            if !Path::new(&machine.binary).is_absolute() {
                bail!("machine {} binary must be an absolute path", machine.name);
            }
        }
        for remote in &self.remotes {
            validate_name(&remote.name)?;
            if !names.insert(remote.name.as_str()) {
                bail!("duplicate machine or remote name {}", remote.name);
            }
            if remote.command.is_empty() {
                bail!("remote {} command cannot be empty", remote.name);
            }
        }
        Ok(())
    }

    pub fn scan_interval_ms(&self) -> u64 {
        self.scan_interval_ms.unwrap_or(300).max(100)
    }

    pub fn collectors(&self) -> Vec<RemoteConfig> {
        self.machines
            .iter()
            .filter(|machine| machine.auto_connect)
            .map(MachineConfig::collector)
            .chain(self.remotes.iter().cloned())
            .collect()
    }

    pub fn machine(&self, name: &str) -> Option<&MachineConfig> {
        self.machines.iter().find(|machine| machine.name == name)
    }
}

impl MachineConfig {
    pub fn focus_command(&self) -> Vec<String> {
        vec![
            "ssh".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=5".into(),
            format!("{}@{}", self.ssh_user, self.host),
            format!("{} remote-focus", shell_quote(&self.binary)),
        ]
    }

    fn collector(&self) -> RemoteConfig {
        let target = format!("{}@{}", self.ssh_user, self.host);
        RemoteConfig {
            name: self.name.clone(),
            command: vec![
                "ssh".into(),
                "-T".into(),
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=5".into(),
                "-o".into(),
                "ServerAliveInterval=15".into(),
                "-o".into(),
                "ServerAliveCountMax=2".into(),
                target,
                format!("{} watch --jsonl --local-only", shell_quote(&self.binary)),
            ],
        }
    }

    pub fn subagent_view_command(&self, target: &str) -> Vec<String> {
        vec![
            "ssh".into(),
            "-tt".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=5".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=2".into(),
            format!("{}@{}", self.ssh_user, self.host),
            format!(
                "{} subagent-view --local-only {}",
                shell_quote(&self.binary),
                shell_quote(target)
            ),
        ]
    }

    pub fn diagnostic_command(&self) -> Vec<String> {
        vec![
            "ssh".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=5".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=2".into(),
            format!("{}@{}", self.ssh_user, self.host),
            format!(
                "{} --version && {} scan --json",
                shell_quote(&self.binary),
                shell_quote(&self.binary)
            ),
        ]
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub socket: PathBuf,
    pub runners: PathBuf,
    pub state: PathBuf,
    pub acknowledgements: PathBuf,
    pub log: PathBuf,
}

impl RuntimePaths {
    pub fn discover(server_key: &str) -> Result<Self> {
        let slug = stable_slug(server_key);
        let runtime_root = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env::temp_dir().join(format!("tmux-agent-{}", unsafe { libc::geteuid() }))
            })
            .join("tmux-agent");
        let socket = socket_path(&runtime_root, &slug);
        let state_root = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
            .context("cannot determine state directory")?
            .join("tmux-agent");
        Ok(Self {
            socket,
            runners: runtime_root.join(format!("{slug}.runs")),
            state: state_root.join(format!("{slug}.json")),
            acknowledgements: state_root.join(format!("{slug}.acknowledged.json")),
            log: state_root.join(format!("{slug}.log")),
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        if let Some(parent) = self.socket.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create runtime directory {}", parent.display()))?;
            #[cfg(unix)]
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("secure runtime directory {}", parent.display()))?;
        }
        fs::create_dir_all(&self.runners)
            .with_context(|| format!("create runner directory {}", self.runners.display()))?;
        #[cfg(unix)]
        fs::set_permissions(&self.runners, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure runner directory {}", self.runners.display()))?;
        if let Some(parent) = self.state.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create state directory {}", parent.display()))?;
            #[cfg(unix)]
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("secure state directory {}", parent.display()))?;
        }
        Ok(())
    }
}

fn socket_path(runtime_root: &Path, slug: &str) -> PathBuf {
    let candidate = runtime_root.join(format!("{slug}.sock"));
    if candidate.as_os_str().as_bytes().len() < 96 {
        return candidate;
    }
    env::temp_dir()
        .join(format!("tmux-agent-{}", unsafe { libc::geteuid() }))
        .join(format!("{slug}.sock"))
}

pub fn default_config_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tmux-agent/config.toml")
}

fn stable_slug(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn enabled_by_default() -> bool {
    true
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("machine or remote name cannot be empty");
    }
    Ok(())
}

fn validate_host(host: &str, machine: &str) -> Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || !host
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".:-_".contains(character))
    {
        bail!("machine {machine} has an invalid SSH host");
    }
    Ok(())
}

fn validate_ssh_user(user: &str, machine: &str) -> Result<()> {
    if user.is_empty()
        || !user
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        bail!("machine {machine} has an invalid SSH user");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_empty_and_valid() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.scan_interval_ms(), 300);
    }

    #[test]
    fn structured_machine_builds_hardened_ssh_collector() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/Users/agent/.local/bin/tmux-agent"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let collectors = config.collectors();
        assert_eq!(collectors.len(), 1);
        assert_eq!(collectors[0].name, "remote-mac");
        assert_eq!(
            collectors[0].command,
            [
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "agent@remote-mac.example.ts.net",
                "'/Users/agent/.local/bin/tmux-agent' watch --jsonl --local-only",
            ]
        );
    }

    #[test]
    fn removed_network_field_is_ignored_for_existing_configs() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                network = "tailscale"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/Users/agent/.local/bin/tmux-agent"
            "#,
        )
        .unwrap();

        assert_eq!(config.collectors()[0].command[0], "ssh");
    }

    #[test]
    fn collector_quotes_remote_binary_paths() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "spaced"
                host = "spaced.example.ts.net"
                ssh_user = "user"
                binary = "/Users/user/My Tools/tmux-agent's copy"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.collectors()[0].command.last().map(String::as_str),
            Some("'/Users/user/My Tools/tmux-agent'\"'\"'s copy' watch --jsonl --local-only")
        );
        let focus = config.machines[0].focus_command();
        assert_eq!(focus[1], "-T");
        assert!(focus.iter().any(|argument| argument == "BatchMode=yes"));
        assert_eq!(
            focus.last().unwrap(),
            "'/Users/user/My Tools/tmux-agent'\"'\"'s copy' remote-focus"
        );
    }

    #[test]
    fn subagent_view_uses_an_interactive_hardened_ssh_session() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/Users/agent/.local/bin/tmux-agent"
            "#,
        )
        .unwrap();
        let command = config
            .machine("remote-mac")
            .unwrap()
            .subagent_view_command("local/terminal/ttys001/10");

        assert_eq!(command[0], "ssh");
        assert_eq!(command[1], "-tt");
        assert!(command.iter().any(|argument| argument == "BatchMode=yes"));
        assert_eq!(
            command.last().map(String::as_str),
            Some(
                "'/Users/agent/.local/bin/tmux-agent' subagent-view --local-only 'local/terminal/ttys001/10'"
            )
        );
    }

    #[test]
    fn machine_diagnostic_reports_version_and_protocol_without_pane_content() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/opt/tmux-agent/bin/tmux-agent"
            "#,
        )
        .unwrap();
        let command = config.machines[0].diagnostic_command();
        let remote = command.last().unwrap();

        assert!(remote.contains("--version"));
        assert!(remote.contains("scan --json"));
        assert!(!remote.contains("capture-pane"));
        assert!(!remote.contains("subagent-view"));
    }

    #[test]
    fn popup_shell_join_quotes_every_argument() {
        assert_eq!(
            shell_join(&["one".into(), "two words".into(), "it's".into()]),
            "'one' 'two words' 'it'\"'\"'s'"
        );
    }

    #[test]
    fn disabled_machine_does_not_start_a_collector() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/Users/agent/.local/bin/tmux-agent"
                auto_connect = false
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.collectors().is_empty());
    }

    #[test]
    fn explicit_remote_commands_remain_supported() {
        let config: Config = toml::from_str(
            r#"
                [[remote]]
                name = "legacy"
                command = ["ssh", "-T", "legacy", "tmux-agent", "watch", "--jsonl"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.collectors()[0].name, "legacy");
    }

    #[test]
    fn duplicate_machine_names_are_rejected() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/Users/agent/.local/bin/tmux-agent"

                [[remote]]
                name = "remote-mac"
                command = ["ssh", "remote-mac"]
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn long_runtime_directories_use_a_short_private_socket_parent() {
        let long_root = PathBuf::from("/tmp").join("a".repeat(120));
        let path = socket_path(&long_root, "0123456789abcdef");

        assert!(!path.starts_with(&long_root));
        assert!(path.ends_with("0123456789abcdef.sock"));
        assert!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tmux-agent-")
        );
    }

    #[test]
    fn multibyte_runtime_directories_are_measured_in_socket_bytes() {
        let long_root = PathBuf::from("/tmp").join("ż".repeat(45));
        let path = socket_path(&long_root, "0123456789abcdef");

        assert!(!path.starts_with(&long_root));
        assert!(path.ends_with("0123456789abcdef.sock"));
    }
}
