use crate::config::{Config, MachineConfig};
use crate::model::{
    AgentRecord, CAPABILITY_REMOTE_FOCUS, ClientConnection, SessionConnections, Snapshot,
};
use crate::tmux::{FocusOutcome, TRANSPORT_ONLY_FOCUS_MESSAGE, Tmux, session_transports};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONTROL_VERSION: u32 = 1;
const CONTROL_LIMIT: u64 = 16 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

pub struct FocusReport {
    pub outcome: FocusOutcome,
    pub notice: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusRequest {
    version: u32,
    server_pid: u32,
    server_started_at: u64,
    session_created_at: u64,
    session_id: String,
    window_id: String,
    pane_id: String,
    client: ClientConnection,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum FocusResponse {
    Selected { target: FocusRequest },
    Rejected { message: String },
}

pub async fn activate(
    tmux: &Tmux,
    config: &Config,
    snapshot: &Snapshot,
    record: &AgentRecord,
) -> Result<FocusReport> {
    activate_with_control(
        tmux,
        config,
        snapshot,
        record,
        |machine, request| async move { send_control(&machine, &request).await },
    )
    .await
}

async fn activate_with_control<F, Fut>(
    tmux: &Tmux,
    config: &Config,
    snapshot: &Snapshot,
    record: &AgentRecord,
    control: F,
) -> Result<FocusReport>
where
    F: FnOnce(MachineConfig, FocusRequest) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let context = tmux.focus_context()?;
    let tmux = &context.tmux;
    let plan = tmux.resolve_agent_focus(record)?;
    let selected = context.select(&plan.target)?;
    let outcome = plan.outcome;
    let partial = |reason: &str| FocusReport {
        outcome,
        notice: format!("{TRANSPORT_ONLY_FOCUS_MESSAGE}; {reason}"),
    };
    if outcome == FocusOutcome::Exact || !record.is_tmux() {
        return Ok(FocusReport {
            outcome,
            notice: String::new(),
        });
    }
    let Some(alias) = record.remote_alias.as_deref() else {
        return Ok(partial("remote identity unavailable"));
    };
    let Some(machine) = config.machine(alias) else {
        return Ok(partial(
            "inner focus requires a structured [[machine]] configuration",
        ));
    };
    if !snapshot.peers.iter().any(|peer| {
        peer.name == alias
            && peer
                .capabilities
                .iter()
                .any(|capability| capability == CAPABILITY_REMOTE_FOCUS)
    }) {
        return Ok(partial("peer does not advertise remote_tmux_focus_v1"));
    }
    let Some(session) = &record.session_connections else {
        return Ok(partial("peer has no live session attachment identity"));
    };
    let Some(transport) = plan.transport else {
        if session.complete {
            bail!("outer transport focused; live client association changed before inner focus");
        }
        return Ok(partial(
            "no inspectable live client association for inner focus",
        ));
    };
    // Resolve the specific remote endpoint that justified this outer pane.
    // A user-maintained binding alone cannot authorize remote selection.
    let clients = session
        .clients
        .iter()
        .filter(|client| {
            let mut candidate = record.clone();
            candidate.session_connections.as_mut().unwrap().clients = vec![(*client).clone()];
            !session_transports(&candidate, std::slice::from_ref(&transport)).is_empty()
        })
        .collect::<Vec<_>>();
    let [client] = clients.as_slice() else {
        return Ok(partial("remote client association is ambiguous"));
    };
    let request = FocusRequest {
        version: CONTROL_VERSION,
        server_pid: session.server_pid,
        server_started_at: session.server_started_at,
        session_created_at: session.session_created_at,
        session_id: record.session_id.clone(),
        window_id: record.window_id.clone(),
        pane_id: record.pane_id.clone(),
        client: (*client).clone(),
    };
    let result = async {
        request.validate()?;
        control(machine.clone(), request.clone()).await?;
        if !tmux.live_focus_transport(record)?.is_some_and(|current| {
            current.target == transport.target
                && current.connection == transport.connection
                && current.mosh_endpoint == transport.mosh_endpoint
        }) {
            bail!("local transport changed during remote selection; refresh and retry");
        }
        // Confirm the original client still shows this target. Never switch
        // again after SSH: the user may have navigated or detached meanwhile.
        context.verify(&selected)?;
        Ok(FocusReport {
            outcome: FocusOutcome::Exact,
            notice: String::new(),
        })
    }
    .await;
    // A failed control operation must not enter the UI's missing-outer-target
    // acknowledgement fallback, even if the final local recheck lost its pane.
    result.map_err(|error: anyhow::Error| {
        anyhow::anyhow!("outer transport focused; inner focus was not confirmed: {error:#}")
    })
}

async fn send_control(machine: &MachineConfig, request: &FocusRequest) -> Result<()> {
    let response = control_output(&machine.focus_command(), request, CONTROL_TIMEOUT).await?;
    confirm_response(&response, request)
}

fn confirm_response(response: &[u8], request: &FocusRequest) -> Result<()> {
    match serde_json::from_slice::<FocusResponse>(response)
        .context("invalid remote focus response")?
    {
        FocusResponse::Selected { target } if target == *request => Ok(()),
        FocusResponse::Selected { .. } => bail!("remote confirmed a different focus target"),
        FocusResponse::Rejected { message } => bail!(
            "remote focus rejected: {}",
            crate::model::terminal_safe(&message)
        ),
    }
}

async fn control_output(
    command: &[String],
    request: &FocusRequest,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(request)?;
    let mut child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // SSH and wrapper output may contain private connection details.
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start SSH focus control")?;
    let operation = async {
        let mut stdin = child
            .stdin
            .take()
            .context("SSH control stdin unavailable")?;
        stdin.write_all(&payload).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let mut response = Vec::new();
        child
            .stdout
            .take()
            .context("SSH control stdout unavailable")?
            .take(CONTROL_LIMIT + 1)
            .read_to_end(&mut response)
            .await?;
        if response.len() as u64 > CONTROL_LIMIT {
            bail!("SSH focus response exceeded size limit");
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!("SSH focus control failed with {status}");
        }
        Ok(response)
    };
    tokio::time::timeout(timeout, operation)
        .await
        .context("SSH focus control timed out")?
}

impl FocusRequest {
    fn validate(&self) -> Result<()> {
        if self.version != CONTROL_VERSION {
            bail!("unsupported remote focus operation version");
        }
        for (value, prefix) in [
            (&self.session_id, '$'),
            (&self.window_id, '@'),
            (&self.pane_id, '%'),
        ] {
            if !value.starts_with(prefix)
                || value.len() < 2
                || !value[1..].bytes().all(|byte| byte.is_ascii_digit())
            {
                bail!("remote focus requires numeric tmux session, window and pane IDs");
            }
        }
        if self.server_pid == 0 || self.server_started_at == 0 || self.session_created_at == 0 {
            bail!("remote focus requires tmux server and session lifetime identity");
        }
        Ok(())
    }

    fn validate_session(&self, session: &SessionConnections) -> Result<()> {
        if session.server_pid != self.server_pid
            || session.server_started_at != self.server_started_at
        {
            bail!(
                "configured SSH control targets a different or restarted tmux server; configure the same server for watch and remote-focus"
            );
        }
        if session.session_created_at != self.session_created_at {
            bail!("remote tmux session was replaced");
        }
        if session
            .clients
            .iter()
            .filter(|client| *client == &self.client)
            .count()
            != 1
        {
            bail!("remote tmux client detached, changed sessions, or is ambiguous");
        }
        Ok(())
    }
}

fn verify_target(tmux: &Tmux, request: &FocusRequest, selected: bool) -> Result<()> {
    let panes = tmux.list_panes()?;
    let processes = tmux.fresh_process_snapshot(&panes)?;
    let sessions = tmux.session_connections(&processes)?;
    let session = sessions
        .get(&request.session_id)
        .context("remote tmux session vanished or configured server is unsupported")?;
    request.validate_session(session)?;
    let clients = tmux.run(&[
        "list-clients",
        "-t",
        &request.session_id,
        "-F",
        "#{client_pid}\t#{client_flags}",
    ])?;
    let matched_flags = clients
        .lines()
        .filter_map(|line| {
            let (pid, flags) = line.split_once('\t')?;
            let pid = pid.parse().ok()?;
            (processes.client_connections.get(&pid) == Some(&request.client)).then_some(flags)
        })
        .collect::<Vec<_>>();
    let [flags] = matched_flags.as_slice() else {
        bail!("remote tmux client detached, changed sessions, or is ambiguous");
    };
    crate::tmux::validate_focus_client_flags(flags)?;
    panes
        .iter()
        .find(|pane| {
            pane.session_id == request.session_id
                && pane.window_id == request.window_id
                && pane.pane_id == request.pane_id
                && !pane.dead
        })
        .context("requested remote tmux window or pane vanished or changed sessions")?;
    if selected {
        let current = tmux.run(&[
            "display-message",
            "-p",
            "-t",
            &format!("{}:", request.session_id),
            "#{window_id} #{pane_id}",
        ])?;
        if current.trim() != format!("{} {}", request.window_id, request.pane_id) {
            bail!("remote tmux target selection was not confirmed");
        }
    }
    Ok(())
}

fn select_remote(tmux: &Tmux, request: &FocusRequest) -> Result<()> {
    request.validate()?;
    verify_target(tmux, request, false)?;
    let window = format!("{}:{}", request.session_id, request.window_id);
    // Window selection is shared by attached clients of this session. Never
    // switch a client to another session, and never use names or shell text.
    tmux.run(&[
        "select-window",
        "-t",
        &window,
        ";",
        "select-pane",
        "-t",
        &request.pane_id,
    ])?;
    verify_target(tmux, request, true)
}

pub fn serve(tmux: &Tmux) -> Result<()> {
    let result = (|| {
        let mut input = Vec::new();
        std::io::stdin()
            .take(CONTROL_LIMIT + 1)
            .read_to_end(&mut input)?;
        if input.len() as u64 > CONTROL_LIMIT {
            bail!("remote focus request exceeded size limit");
        }
        let request: FocusRequest =
            serde_json::from_slice(&input).context("invalid remote focus request")?;
        select_remote(tmux, &request)?;
        Ok(request)
    })();
    let response = match result {
        Ok(target) => FocusResponse::Selected { target },
        Err(error) => FocusResponse::Rejected {
            message: format!("{error:#}"),
        },
    };
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::shell_join;
    use crate::model::MoshEndpoint;
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Write;
    use std::process::Command;
    use std::time::Instant;

    fn request() -> FocusRequest {
        FocusRequest {
            version: CONTROL_VERSION,
            server_pid: 42,
            server_started_at: 100,
            session_created_at: 200,
            session_id: "$1".into(),
            window_id: "@2".into(),
            pane_id: "%3".into(),
            client: ClientConnection::Mosh {
                endpoint: MoshEndpoint {
                    address: "127.0.0.1".into(),
                    port: 60001,
                },
            },
        }
    }

    #[test]
    fn operation_requires_numeric_ids_and_exact_confirmation() {
        let request = request();
        request.validate().unwrap();
        for invalid in [
            "session name",
            "$1; new-window",
            "$1\n",
            "$",
            "$(touch file)",
            "-t",
        ] {
            let mut invalid_request = request.clone();
            invalid_request.session_id = invalid.into();
            assert!(invalid_request.validate().is_err());
        }
        let selected = serde_json::to_vec(&FocusResponse::Selected {
            target: request.clone(),
        })
        .unwrap();
        confirm_response(&selected, &request).unwrap();
        let mut different = request.clone();
        different.pane_id = "%4".into();
        assert!(confirm_response(&selected, &different).is_err());
        assert!(confirm_response(b"{}", &request).is_err());
        assert!(
            confirm_response(
                br#"{"result":"rejected","message":"target vanished"}"#,
                &request
            )
            .unwrap_err()
            .to_string()
            .contains("target vanished")
        );
        assert!(confirm_response(br#"{"result":"selected","target":{}}"#, &request).is_err());
    }

    #[tokio::test]
    async fn control_handles_stdin_bounds_failure_and_full_lifetime_timeout() {
        let request = request();
        let command = |script: &str| vec!["sh".into(), "-c".into(), script.into()];
        let output = control_output(&command("cat"), &request, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<FocusRequest>(&output).unwrap(),
            request
        );
        assert!(
            control_output(
                &command("cat >/dev/null; exit 7"),
                &request,
                Duration::from_secs(2)
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("failed")
        );
        assert!(
            control_output(
                &command("cat >/dev/null; head -c 20000 /dev/zero"),
                &request,
                Duration::from_secs(2)
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("size limit")
        );
        let started = Instant::now();
        assert!(
            control_output(
                &command("cat >/dev/null; exec sleep 10"),
                &request,
                Duration::from_millis(100)
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn real_mosh_inner_selection_validates_lifetime_session_client_and_target() {
        const FIXTURE: &str = "TMUX_AGENT_INNER_FOCUS_TEST";
        if std::env::var_os(FIXTURE).is_none() {
            for program in ["tmux", "mosh-client", "mosh-server", "python3", "lsof"] {
                if Command::new(program).arg("--version").output().is_err() {
                    eprintln!("skipping real inner-focus test: {program} unavailable");
                    return;
                }
            }
            let directory = tempfile::tempdir().unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "focus::tests::real_mosh_inner_selection_validates_lifetime_session_client_and_target", "--nocapture"])
                .env(FIXTURE, "1").env("TMUX_TMPDIR", directory.path())
                .env("TMUX", "isolated-focus-test,0,0").env("TMUX_PANE", "%0").status().unwrap();
            assert!(status.success());
            return;
        }
        struct Servers(Vec<String>);
        impl Drop for Servers {
            fn drop(&mut self) {
                for name in &self.0 {
                    let _ = Command::new("tmux")
                        .args(["-L", name, "kill-server"])
                        .output();
                }
            }
        }
        let outer_name = format!("inner-focus-outer-{}", std::process::id());
        let inner_name = format!("inner-focus-remote-{}", std::process::id());
        let _servers = Servers(vec![outer_name.clone(), inner_name.clone()]);
        let outer = Tmux::new(&Config {
            tmux_args: vec!["-L".into(), outer_name],
            ..Config::default()
        });
        let inner = Tmux::new(&Config {
            tmux_args: vec!["-L".into(), inner_name.clone()],
            ..Config::default()
        });
        inner
            .run(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "session with spaces",
                "-x",
                "100",
                "-y",
                "40",
                "sleep 90",
            ])
            .unwrap();
        inner
            .run(&["set-option", "-g", "set-titles", "on"])
            .unwrap();
        let target_window = inner
            .run(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{window_id}",
                "-t",
                "session with spaces:",
                "sleep 90",
            ])
            .unwrap()
            .trim()
            .to_string();
        let target_pane = inner
            .run(&[
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &target_window,
                "sleep 90",
            ])
            .unwrap()
            .trim()
            .to_string();
        inner
            .run(&["new-session", "-d", "-s", "unrelated", "sleep 90"])
            .unwrap();
        let attach = shell_join(&[
            "python3".into(),
            format!(
                "{}/tests/fixtures/mosh-attachment.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            inner_name,
            "session with spaces".into(),
        ]);
        outer
            .run(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "ui",
                "sleep 90",
            ])
            .unwrap();
        outer
            .run(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "outer",
                "-x",
                "100",
                "-y",
                "40",
                &attach,
            ])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let session = loop {
            let panes = inner.list_panes().unwrap();
            let processes = inner.fresh_process_snapshot(&panes).unwrap();
            let mut sessions = inner.session_connections(&processes).unwrap();
            let session = sessions.remove("$0").unwrap();
            if session.clients.len() == 1 {
                break session;
            }
            assert!(Instant::now() < deadline, "Mosh did not attach");
            std::thread::sleep(Duration::from_millis(50));
        };
        let request = FocusRequest {
            server_pid: session.server_pid,
            server_started_at: session.server_started_at,
            session_created_at: session.session_created_at,
            session_id: "$0".into(),
            window_id: target_window,
            pane_id: target_pane,
            client: session.clients[0].clone(),
            ..request()
        };
        check_activation_clients(&outer, &inner, &request, session);
        let current = || {
            inner
                .run(&[
                    "display-message",
                    "-p",
                    "-t",
                    "$0:",
                    "#{window_id} #{pane_id}",
                ])
                .unwrap()
        };
        let original = current();
        for field in [
            "server",
            "lifetime",
            "session",
            "window",
            "pane",
            "client",
            "injection",
        ] {
            let mut stale = request.clone();
            match field {
                "server" => stale.server_pid += 1,
                "lifetime" => stale.server_started_at += 1,
                "session" => stale.session_created_at += 1,
                "window" => stale.window_id = "@9999".into(),
                "pane" => stale.pane_id = "%9999".into(),
                "client" => match &mut stale.client {
                    ClientConnection::Mosh { endpoint } => {
                        endpoint.port = endpoint.port.wrapping_add(1)
                    }
                    ClientConnection::Ssh { connection } => {
                        connection.client_port = connection.client_port.wrapping_add(1)
                    }
                },
                "injection" => stale.pane_id = "%1; new-window".into(),
                _ => unreachable!(),
            }
            assert!(select_remote(&inner, &stale).is_err(), "accepted {field}");
            assert_eq!(current(), original, "mutated before rejecting {field}");
        }
        select_remote(&inner, &request).unwrap();
        assert_eq!(
            current().trim(),
            format!("{} {}", request.window_id, request.pane_id)
        );
        let client = inner
            .run(&["list-clients", "-t", "$0", "-F", "#{client_name}"])
            .unwrap();
        let other_pane = inner
            .run(&["list-panes", "-t", &request.window_id, "-F", "#{pane_id}"])
            .unwrap()
            .lines()
            .find(|pane| *pane != request.pane_id)
            .unwrap()
            .to_string();
        inner
            .run(&["refresh-client", "-t", client.trim(), "-f", "active-pane"])
            .unwrap();
        inner
            .run(&["bind-key", "-n", "F11", "select-pane", "-t", &other_pane])
            .unwrap();
        inner
            .run(&[
                "bind-key",
                "-n",
                "F12",
                "set-option",
                "-gF",
                "@focus_test_client_pane",
                "#{pane_id}",
            ])
            .unwrap();
        outer
            .run(&["send-keys", "-t", "outer:0", "F11", "F12"])
            .unwrap();
        wait_until_ready("independent remote pane selection", || {
            inner
                .run(&["show-option", "-gqv", "@focus_test_client_pane"])
                .unwrap()
                .trim()
                == other_pane
        });
        inner.run(&["select-window", "-t", "$0:0"]).unwrap();
        let before_rejection = current();
        let selected = select_remote(&inner, &request);
        inner
            .run(&["set-option", "-gu", "@focus_test_client_pane"])
            .unwrap();
        outer.run(&["send-keys", "-t", "outer:0", "F12"]).unwrap();
        wait_until_ready("remote client pane observation", || {
            !inner
                .run(&["show-option", "-gqv", "@focus_test_client_pane"])
                .unwrap()
                .trim()
                .is_empty()
        });
        let actual = inner
            .run(&["show-option", "-gqv", "@focus_test_client_pane"])
            .unwrap();
        assert!(
            selected
                .as_ref()
                .is_err_and(|error| error.to_string().contains("active-pane")),
            "independent client pane was not selected, yet remote focus confirmed success: {}",
            actual.trim()
        );
        assert_eq!(
            current(),
            before_rejection,
            "unsupported client mutated before rejection"
        );
        inner
            .run(&["refresh-client", "-t", client.trim(), "-f", "!active-pane"])
            .unwrap();
        inner.run(&["select-pane", "-t", &other_pane]).unwrap();
        let change_mode = shell_join(&[
            "refresh-client".into(),
            "-t".into(),
            client.trim().into(),
            "-f".into(),
            "active-pane".into(),
        ]);
        inner
            .run(&["set-hook", "-g", "after-select-pane", &change_mode])
            .unwrap();
        let changed_mode = select_remote(&inner, &request).unwrap_err();
        assert!(
            changed_mode.to_string().contains("active-pane"),
            "{changed_mode:#}"
        );
        inner
            .run(&["set-hook", "-gu", "after-select-pane"])
            .unwrap();
        inner
            .run(&["refresh-client", "-t", client.trim(), "-f", "!active-pane"])
            .unwrap();
        inner
            .run(&["switch-client", "-c", client.trim(), "-t", "unrelated"])
            .unwrap();
        assert!(
            select_remote(&inner, &request)
                .unwrap_err()
                .to_string()
                .contains("client")
        );
        assert_eq!(
            inner
                .run(&["list-clients", "-F", "#{session_name}"])
                .unwrap()
                .trim(),
            "unrelated"
        );
    }

    fn wait_until_ready(description: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn check_activation_clients(
        outer: &Tmux,
        inner: &Tmux,
        request: &FocusRequest,
        session: SessionConnections,
    ) {
        let socket = outer.server_key().unwrap().unwrap();
        let mut clients = Vec::new();
        for _ in 0..2 {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 40,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let mut command = CommandBuilder::new("tmux");
            command.args(["-S", &socket, "attach-session", "-t", "ui"]);
            command.env_remove("TMUX");
            command.env_remove("TMUX_PANE");
            command.env("TERM", "xterm-256color");
            let child = pair.slave.spawn_command(command).unwrap();
            drop(pair.slave);
            let pid = child.process_id().unwrap().to_string();
            clients.push((child, pair.master));
            wait_until_ready("tmux client attachment", || {
                outer
                    .run(&["list-clients", "-F", "#{client_pid}"])
                    .unwrap()
                    .lines()
                    .any(|line| line == pid)
            });
        }
        let initiating = clients[0].0.process_id().unwrap();
        let spectator = clients[1].0.process_id().unwrap();
        // The first client supplies input after the spectator attaches second.
        let mut input = clients[0].1.take_writer().unwrap();
        let mut initiating_input = || {
            input.write_all(b"x").unwrap();
            wait_until_ready("initiating client input", || {
                outer
                    .run(&["display-message", "-p", "#{client_pid}"])
                    .unwrap()
                    .trim()
                    == initiating.to_string()
            });
        };
        initiating_input();
        let record = AgentRecord {
            id: "remote/test/target".into(),
            host: "test".into(),
            server: "default".into(),
            pane_id: request.pane_id.clone(),
            pane_pid: 1,
            session_id: request.session_id.clone(),
            session_name: "session with spaces".into(),
            window_id: request.window_id.clone(),
            window_index: 1,
            window_name: "hidden".into(),
            pane_index: 1,
            agent: "Claude".into(),
            state: Default::default(),
            attention: Default::default(),
            source: Default::default(),
            title: "synthetic target".into(),
            label: None,
            cwd: "/".into(),
            visible: false,
            seen: true,
            changed_at_ms: 1,
            origin: Default::default(),
            terminal: None,
            remote_alias: Some("test".into()),
            ssh_connection: None,
            session_connections: Some(session),
            focus_target: None,
            goal: None,
            subagent: None,
            detection: None,
        };
        let config = Config {
            machines: vec![MachineConfig {
                name: "test".into(),
                host: "localhost".into(),
                ssh_user: "test".into(),
                binary: "/test/tmux-agent".into(),
                auto_connect: false,
            }],
            ..Config::default()
        };
        let snapshot = Snapshot {
            peers: vec![crate::model::PeerStatus {
                name: "test".into(),
                capabilities: vec![CAPABILITY_REMOTE_FOCUS.into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report = runtime
            .block_on(activate_with_control(
                outer,
                &config,
                &snapshot,
                &record,
                |_, target| async move { select_remote(inner, &target) },
            ))
            .unwrap();
        assert_eq!(report.outcome, FocusOutcome::Exact);
        let selected = outer
            .run(&["list-clients", "-F", "#{client_pid}:#{session_name}"])
            .unwrap();
        assert!(
            selected
                .lines()
                .any(|line| line == format!("{initiating}:outer")),
            "initiator did not move: {selected}"
        );
        assert!(
            selected
                .lines()
                .any(|line| line == format!("{spectator}:ui")),
            "spectator moved: {selected}"
        );
        let client_name = |pid: u32| {
            outer
                .run(&["list-clients", "-F", "#{client_pid}\t#{client_name}"])
                .unwrap()
                .lines()
                .find_map(|line| {
                    let (current, name) = line.split_once('\t')?;
                    (current == pid.to_string()).then(|| name.to_string())
                })
                .unwrap()
        };
        let initiating_name = client_name(initiating);
        let spectator_name = client_name(spectator);
        let alternate = outer
            .run(&[
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "outer:0",
                "sleep 90",
            ])
            .unwrap()
            .trim()
            .to_string();
        for case in [
            "failed-control",
            "changed-session",
            "changed-pane",
            "old-peer",
            "independent-local",
            "changed-client-mode",
            "local",
            "single-client",
            "detach",
        ] {
            outer
                .run(&["switch-client", "-c", &initiating_name, "-t", "ui"])
                .unwrap();
            initiating_input();
            inner.run(&["select-window", "-t", "$0:0"]).unwrap();
            let mut selected_record = record.clone();
            let mut selected_snapshot = snapshot.clone();
            if case == "old-peer" {
                selected_snapshot.peers.clear();
            }
            if case == "independent-local" {
                outer
                    .run(&[
                        "refresh-client",
                        "-t",
                        &initiating_name,
                        "-f",
                        "active-pane",
                    ])
                    .unwrap();
            }
            if case == "local" {
                let target = outer
                    .list_panes()
                    .unwrap()
                    .into_iter()
                    .find(|pane| pane.pane_id == alternate)
                    .unwrap();
                selected_record.remote_alias = None;
                selected_record.session_connections = None;
                selected_record.session_name = target.session_name;
                selected_record.session_id = target.session_id;
                selected_record.window_id = target.window_id;
                selected_record.pane_id = target.pane_id;
            }
            if case == "single-client" {
                outer
                    .run(&["detach-client", "-t", &spectator_name])
                    .unwrap();
            }
            let result = runtime.block_on(activate_with_control(
                outer,
                &config,
                &selected_snapshot,
                &selected_record,
                |_, target| {
                    let initiating_name = &initiating_name;
                    let alternate = &alternate;
                    async move {
                        assert!(!matches!(case, "old-peer" | "local" | "independent-local"));
                        if case == "failed-control" {
                            bail!("synthetic control rejection")
                        }
                        select_remote(inner, &target)?;
                        match case {
                            "changed-session" => {
                                outer.run(&["switch-client", "-c", initiating_name, "-t", "ui"])?;
                            }
                            "changed-pane" => {
                                outer.run(&["select-pane", "-t", alternate])?;
                            }
                            "detach" => {
                                outer.run(&["detach-client", "-t", initiating_name])?;
                            }
                            "changed-client-mode" => {
                                outer.run(&[
                                    "refresh-client",
                                    "-t",
                                    initiating_name,
                                    "-f",
                                    "active-pane",
                                ])?;
                            }
                            _ => {}
                        }
                        Ok(())
                    }
                },
            ));
            if matches!(case, "old-peer" | "local" | "single-client") {
                assert_eq!(
                    result.unwrap().outcome,
                    if case == "old-peer" {
                        FocusOutcome::TransportOnly
                    } else {
                        FocusOutcome::Exact
                    },
                    "{case}"
                );
            } else if case == "independent-local" {
                assert!(result.err().unwrap().to_string().contains("active-pane"));
            } else {
                let error = result.err().unwrap();
                assert!(!crate::tmux::is_focus_target_missing(&error));
                let error = error.to_string();
                assert!(
                    error.contains("inner focus was not confirmed"),
                    "{case}: {error}"
                );
            }
            let selected = outer
                .run(&["list-clients", "-F", "#{client_pid}:#{session_name}"])
                .unwrap();
            if !matches!(case, "single-client" | "detach") {
                assert!(
                    selected
                        .lines()
                        .any(|line| line == format!("{spectator}:ui")),
                    "{case} moved spectator: {selected}"
                );
            }
            if matches!(case, "changed-session" | "independent-local") {
                assert!(
                    selected
                        .lines()
                        .any(|line| line == format!("{initiating}:ui")),
                    "focus undid user navigation"
                );
            }
            if case == "changed-pane" {
                assert_eq!(
                    outer
                        .run(&["display-message", "-p", "-t", "outer:", "#{pane_id}"])
                        .unwrap()
                        .trim(),
                    alternate,
                    "focus undid user pane selection"
                );
            }
            if case == "detach" {
                assert!(
                    !selected
                        .lines()
                        .any(|line| line.starts_with(&format!("{initiating}:")))
                );
            }
            if matches!(case, "independent-local" | "changed-client-mode") {
                outer
                    .run(&[
                        "refresh-client",
                        "-t",
                        &initiating_name,
                        "-f",
                        "!active-pane",
                    ])
                    .unwrap();
            }
        }
        for (mut child, _) in clients {
            let _ = child.kill();
            child.wait().unwrap();
        }
        inner.run(&["select-window", "-t", "$0:0"]).unwrap();
    }
}
