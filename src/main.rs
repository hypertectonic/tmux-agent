mod codex;
mod config;
mod daemon;
mod detect;
mod doctor;
mod ipc;
mod model;
mod runner;
mod scanner;
mod store;
mod tmux;
mod transcript;
mod ui;
mod update;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, RuntimePaths};
use model::{Snapshot, terminal_safe};
use scanner::Scanner;
use std::ffi::OsString;
use std::path::PathBuf;
use tmux::Tmux;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run or inspect the background collector.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Show the latest discovered agents.
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        local_only: bool,
    },
    /// Stream snapshots for an SSH federation peer.
    Watch {
        #[arg(long)]
        jsonl: bool,
        #[arg(long)]
        local_only: bool,
    },
    /// Open the interactive agent view in the current terminal.
    Ui {
        /// Adapt lifecycle for a tmux popup: do not mark a pane, exit after exact
        /// focus, and remain open to report transport-only focus.
        #[arg(long)]
        popup: bool,
    },
    /// Focus an agent by full ID or an unambiguous ID suffix.
    Focus { target: String },
    /// Explain the current evidence and state for an agent.
    Explain { target: String },
    /// Mark an agent's completion as seen.
    Acknowledge { target: String },
    /// Manage explicit local-pane mappings for remote tmux sessions.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Open the internal read-only Codex subagent transcript viewer.
    #[command(name = "subagent-view", hide = true)]
    SubagentView {
        target: String,
        #[arg(long)]
        local_only: bool,
    },
    /// Run one local scan without starting the daemon.
    Scan {
        #[arg(long)]
        json: bool,
    },
    /// Run an agent through an owned PTY for screen-based state detection.
    #[command(after_help = "Examples:
  tmux-agent run -- codex
  tmux-agent run -- codex resume
  tmux-agent run -- codex resume <session-id>
  tmux-agent run -- claude
  tmux-agent run -- opencode
  tmux-agent run -- omp
  tmux-agent run -- pi")]
    Run {
        /// Agent command to proxy, for example: codex resume [session-id].
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<OsString>,
    },
    /// Run Codex through the owned PTY without replacing the codex command.
    #[command(disable_help_flag = true)]
    Codex {
        /// Arguments forwarded directly to Codex.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Run Claude through the owned PTY without replacing the claude command.
    #[command(disable_help_flag = true)]
    Claude {
        /// Arguments forwarded directly to Claude.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Run OpenCode through the owned PTY without replacing the opencode command.
    #[command(disable_help_flag = true)]
    Opencode {
        /// Arguments forwarded directly to OpenCode.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Run OMP through the owned PTY without replacing the omp command.
    #[command(disable_help_flag = true)]
    Omp {
        /// Arguments forwarded directly to OMP.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Run Pi through the owned PTY without replacing the pi command.
    #[command(disable_help_flag = true)]
    Pi {
        /// Arguments forwarded directly to Pi.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Print the resolved config, socket, state, and log paths.
    Paths,
    /// Check local setup, daemon health, and configured SSH peers.
    Doctor {
        /// Emit a stable machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Install and activate a verified tmux-agent release.
    Update {
        /// Install one exact version; prereleases must be requested this way.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },
    /// List the active managed version and available rollback targets.
    Versions,
    /// Activate one already installed managed version.
    Rollback {
        /// Exact installed semantic version to activate.
        version: String,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run,
    /// Ensure a daemon is running and print its status.
    Start,
    /// Print the daemon status.
    Status,
    /// Stop the daemon cleanly.
    Stop,
    /// Stop the current daemon and start it again with this binary.
    Restart,
}

#[derive(Debug, Subcommand)]
enum RemoteCommand {
    /// Bind a local tmux pane to one configured remote tmux session.
    Bind {
        remote: String,
        session: String,
        /// Local pane ID. Defaults to the current local TMUX_PANE.
        #[arg(long, value_name = "PANE_ID")]
        pane: Option<String>,
    },
    /// Remove a remote-session binding from a local tmux pane.
    Unbind {
        /// Local pane ID. Defaults to the current local TMUX_PANE.
        #[arg(long, value_name = "PANE_ID")]
        pane: Option<String>,
    },
    /// List explicit local-pane mappings for remote tmux sessions.
    Bindings,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Command::Doctor { json } = &cli.command {
        return doctor::run(cli.config.as_deref(), *json).await;
    }
    if let Command::Update { version } = &cli.command {
        return update::run(version.as_deref(), cli.config.as_deref());
    }
    if matches!(cli.command, Command::Versions) {
        return update::run_versions();
    }
    if let Command::Rollback { version } = &cli.command {
        return update::run_rollback(version, cli.config.as_deref());
    }

    let (config, config_path) = Config::load(cli.config.as_deref())?;
    let tmux = Tmux::new(&config);
    let paths = RuntimePaths::discover(&tmux.runtime_key())?;

    match cli.command {
        Command::Doctor { .. } => unreachable!(),
        Command::Update { .. } => unreachable!(),
        Command::Versions | Command::Rollback { .. } => unreachable!(),
        Command::Daemon {
            command: DaemonCommand::Run,
        } => daemon::run(config, paths, tmux).await,
        Command::Daemon {
            command: DaemonCommand::Start,
        } => {
            daemon::ensure_running(&config_path, &paths).await?;
            let snapshot = ipc::snapshot(&paths.socket, false).await?;
            println!(
                "running: version {}, protocol {}, {} agents, socket {}",
                snapshot.application_version.as_deref().unwrap_or("unknown"),
                snapshot.protocol,
                snapshot.agents.len(),
                paths.socket.display()
            );
            Ok(())
        }
        Command::Daemon {
            command: DaemonCommand::Status,
        } => match ipc::snapshot(&paths.socket, false).await {
            Ok(snapshot) => {
                println!(
                    "running: version {}, protocol {}, revision {}, {} agents, {} peers",
                    snapshot.application_version.as_deref().unwrap_or("unknown"),
                    snapshot.protocol,
                    snapshot.revision,
                    snapshot.agents.len(),
                    snapshot.peers.len()
                );
                Ok(())
            }
            Err(_) => {
                println!("stopped: {}", paths.socket.display());
                Ok(())
            }
        },
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => {
            ipc::shutdown(&paths.socket).await?;
            println!("stopped: {}", paths.socket.display());
            Ok(())
        }
        Command::Daemon {
            command: DaemonCommand::Restart,
        } => {
            if ipc::snapshot(&paths.socket, false).await.is_ok() {
                ipc::shutdown(&paths.socket).await?;
            }
            daemon::ensure_running(&config_path, &paths).await?;
            let snapshot = ipc::snapshot(&paths.socket, false).await?;
            println!(
                "restarted: version {}, revision {}, socket {}",
                snapshot.application_version.as_deref().unwrap_or("unknown"),
                snapshot.revision,
                paths.socket.display()
            );
            Ok(())
        }
        Command::List { json, local_only } => {
            daemon::ensure_running(&config_path, &paths).await?;
            let snapshot = ipc::snapshot(&paths.socket, local_only).await?;
            print_snapshot(&snapshot, json)
        }
        Command::Watch { jsonl, local_only } => {
            if !jsonl {
                bail!("watch currently requires --jsonl");
            }
            daemon::ensure_running(&config_path, &paths).await?;
            ipc::watch_jsonl(&paths.socket, local_only).await
        }
        Command::Ui { popup } => {
            daemon::ensure_running(&config_path, &paths).await?;
            ui::run(&paths, tmux, popup, &config, &config_path).await
        }
        Command::Focus { target } => {
            daemon::ensure_running(&config_path, &paths).await?;
            let snapshot = ipc::snapshot(&paths.socket, false).await?;
            let record = resolve(&snapshot, &target)?;
            tmux.focus_agent(record).map(|_| ())
        }
        Command::Explain { target } => {
            daemon::ensure_running(&config_path, &paths).await?;
            let snapshot = ipc::snapshot(&paths.socket, false).await?;
            let record = resolve(&snapshot, &target)?;
            println!("{}", serde_json::to_string_pretty(record)?);
            Ok(())
        }
        Command::Acknowledge { target } => {
            daemon::ensure_running(&config_path, &paths).await?;
            let snapshot = ipc::snapshot(&paths.socket, false).await?;
            let id = resolve(&snapshot, &target)?.id.clone();
            ipc::acknowledge(&paths.socket, &id).await?;
            println!("acknowledged {id}");
            Ok(())
        }
        Command::Remote {
            command:
                RemoteCommand::Bind {
                    remote,
                    session,
                    pane,
                },
        } => {
            if !configured_remote(&config, &remote) {
                bail!("no configured remote named {remote:?}");
            }
            if session.trim().is_empty() {
                bail!("remote tmux session cannot be empty");
            }
            let pane_id = tmux.bind_remote_pane(pane.as_deref(), &remote, &session)?;
            println!(
                "bound {} to {}/{}",
                terminal_safe(&pane_id),
                terminal_safe(&remote),
                terminal_safe(&session)
            );
            Ok(())
        }
        Command::Remote {
            command: RemoteCommand::Unbind { pane },
        } => {
            let pane_id = tmux.unbind_remote_pane(pane.as_deref())?;
            println!("unbound {pane_id}");
            Ok(())
        }
        Command::Remote {
            command: RemoteCommand::Bindings,
        } => {
            let bindings = tmux.remote_pane_bindings()?;
            if bindings.is_empty() {
                println!("No remote pane bindings.");
            } else {
                for binding in bindings {
                    println!(
                        "{} {} {}",
                        terminal_safe(&binding.pane_id),
                        terminal_safe(&binding.remote),
                        terminal_safe(&binding.session)
                    );
                }
            }
            Ok(())
        }
        Command::SubagentView { target, local_only } => {
            daemon::ensure_running(&config_path, &paths).await?;
            let snapshot = ipc::snapshot(&paths.socket, local_only).await?;
            let record = resolve(&snapshot, &target)?;
            transcript::run(record)
        }
        Command::Scan { json } => {
            let discovered_server_key = tmux.server_key()?;
            let tmux_server_observed = discovered_server_key.is_some();
            let server_key = discovered_server_key.unwrap_or_else(|| tmux.runtime_key());
            let persisted = store::load(&paths.state).unwrap_or_default();
            let mut scanner = Scanner::new(
                &config,
                tmux,
                &server_key,
                paths.runners.clone(),
                persisted,
                tmux_server_observed,
            )?;
            let snapshot = scanner.scan()?;
            print_snapshot(&snapshot, json)
        }
        Command::Run { command } => run_owned_pty(command, &paths),
        Command::Codex { arguments } => run_owned_pty(provider_command("codex", arguments), &paths),
        Command::Claude { arguments } => {
            run_owned_pty(provider_command("claude", arguments), &paths)
        }
        Command::Opencode { arguments } => {
            run_owned_pty(provider_command("opencode", arguments), &paths)
        }
        Command::Omp { arguments } => run_owned_pty(provider_command("omp", arguments), &paths),
        Command::Pi { arguments } => run_owned_pty(provider_command("pi", arguments), &paths),
        Command::Paths => {
            println!("config {}", config_path.display());
            println!("socket {}", paths.socket.display());
            println!("runs   {}", paths.runners.display());
            println!("state  {}", paths.state.display());
            println!("acks   {}", paths.acknowledgements.display());
            println!("log    {}", paths.log.display());
            Ok(())
        }
    }
}

fn configured_remote(config: &Config, name: &str) -> bool {
    config.machines.iter().any(|machine| machine.name == name)
        || config.remotes.iter().any(|remote| remote.name == name)
}

fn provider_command(provider: &str, arguments: Vec<OsString>) -> Vec<OsString> {
    std::iter::once(OsString::from(provider))
        .chain(arguments)
        .collect()
}

fn run_owned_pty(command: Vec<OsString>, paths: &RuntimePaths) -> Result<()> {
    let exit_code = runner::run(command, paths)?;
    if exit_code == 0 {
        Ok(())
    } else {
        std::process::exit(exit_code);
    }
}

fn print_snapshot(snapshot: &Snapshot, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(snapshot)?);
        return Ok(());
    }
    if snapshot.agents.is_empty() {
        println!("No supported agent sessions found.");
    } else {
        println!(
            "{:<2} {:<8} {:<18} {:<22} TITLE",
            "", "STATE", "AGENT@HOST", "LOCATION"
        );
        for agent in &snapshot.agents {
            println!(
                "{:<2} {:<8} {:<18} {:<22} {}",
                agent.attention.icon(),
                format!("{:?}", agent.attention).to_lowercase(),
                terminal_safe(&format!("{}@{}", agent.agent, agent.host)),
                terminal_safe(&agent.location_label()),
                terminal_safe(&agent.title)
            );
        }
    }
    for peer in &snapshot.peers {
        if peer.connected {
            println!(
                "peer {}: connected, version {}, protocol {}",
                terminal_safe(&peer.name),
                peer.application_version.as_deref().unwrap_or("unknown"),
                peer.protocol
            );
        } else {
            println!(
                "peer {}: disconnected{}",
                terminal_safe(&peer.name),
                peer.last_error
                    .as_deref()
                    .map(|error| format!(" ({})", terminal_safe(error)))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn resolve<'a>(snapshot: &'a Snapshot, target: &str) -> Result<&'a crate::model::AgentRecord> {
    let matches = snapshot
        .agents
        .iter()
        .filter(|agent| {
            agent.id == target
                || agent.id.ends_with(target)
                || agent.pane_id == target
                || format!("{}:{}", agent.host, agent.pane_id) == target
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [record] => Ok(record),
        [] => bail!("no agent matches {target:?}"),
        _ => bail!("agent target {target:?} is ambiguous"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn help_documents_supported_agents_through_the_runner() {
        let mut command = Cli::command();
        let root_help = command.render_long_help().to_string();
        assert!(root_help.contains("codex"));
        assert!(root_help.contains("claude"));
        assert!(root_help.contains("opencode"));
        assert!(root_help.contains("pi"));
        assert!(command.find_subcommand("omp").is_some());

        let command = Cli::command();
        let run = command
            .find_subcommand("run")
            .expect("run subcommand should exist");
        let help = run.clone().render_long_help().to_string();
        assert!(help.contains("tmux-agent run -- codex resume"));
        assert!(help.contains("tmux-agent run -- codex resume <session-id>"));
        assert!(help.contains("tmux-agent run -- claude"));
        assert!(help.contains("tmux-agent run -- opencode"));
        assert!(help.contains("tmux-agent run -- omp"));
        assert!(help.contains("tmux-agent run -- pi"));

        let command = Cli::command();
        let ui = command
            .find_subcommand("ui")
            .expect("ui subcommand should exist");
        let help = ui.clone().render_long_help().to_string();
        assert!(
            help.contains("exit after exact focus, and remain open to report transport-only focus")
        );
    }

    #[test]
    fn provider_shortcuts_forward_all_arguments_without_a_separator() {
        let codex = Cli::try_parse_from([
            "tmux-agent",
            "codex",
            "resume",
            "session-id",
            "--model",
            "gpt-test",
        ])
        .unwrap();
        let Command::Codex { arguments } = codex.command else {
            panic!("expected codex shortcut");
        };
        assert_eq!(
            provider_command("codex", arguments),
            ["codex", "resume", "session-id", "--model", "gpt-test"].map(OsString::from)
        );

        let claude =
            Cli::try_parse_from(["tmux-agent", "claude", "--continue", "--model", "sonnet"])
                .unwrap();
        let Command::Claude { arguments } = claude.command else {
            panic!("expected claude shortcut");
        };
        assert_eq!(
            provider_command("claude", arguments),
            ["claude", "--continue", "--model", "sonnet"].map(OsString::from)
        );
    }

    #[test]
    fn provider_shortcuts_allow_no_arguments_and_forward_help() {
        let opencode = Cli::try_parse_from(["tmux-agent", "opencode"]).unwrap();
        let Command::Opencode { arguments } = opencode.command else {
            panic!("expected opencode shortcut");
        };
        assert_eq!(
            provider_command("opencode", arguments),
            [OsString::from("opencode")]
        );

        let pi = Cli::try_parse_from(["tmux-agent", "pi", "--help"]).unwrap();
        let Command::Pi { arguments } = pi.command else {
            panic!("expected pi shortcut");
        };
        assert_eq!(
            provider_command("pi", arguments),
            [OsString::from("pi"), OsString::from("--help")]
        );

        let codex = Cli::try_parse_from(["tmux-agent", "codex", "--help"]).unwrap();
        let Command::Codex { arguments } = codex.command else {
            panic!("expected codex shortcut");
        };
        assert_eq!(
            provider_command("codex", arguments),
            [OsString::from("codex"), OsString::from("--help")]
        );
    }

    #[test]
    fn release_version_matches_cargo_metadata_and_lockfile() {
        let version = include_str!("../VERSION").trim();
        assert_eq!(version, env!("CARGO_PKG_VERSION"));

        let lockfile: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
        let package = lockfile["package"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["name"].as_str() == Some("tmux-agent"))
            .expect("tmux-agent package should exist in Cargo.lock");
        assert_eq!(package["version"].as_str(), Some(version));
    }
}
