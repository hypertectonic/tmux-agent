//! Agent-to-agent handoff.
//!
//! The sending machine resolves exactly one target agent from its federated
//! snapshot and pastes a scoped message into that agent's pane, either on the
//! local tmux server or through the configured SSH control command of the peer
//! that owns the pane. The receiving side revalidates the target against live
//! tmux and its own scan before pasting, so a different server, a replaced
//! session, a recreated pane, a changed foreground process lifetime, and a
//! prompt waiting for input fail closed instead of receiving text. Delivery is synchronous:
//! there is no mailbox, queue, or retry loop. A sender retries with the same
//! handoff ID and the receiver answers an already delivered ID as a duplicate.
use crate::config::{Config, MachineConfig, RuntimePaths};
use crate::focus::ControlExit;
use crate::model::{AgentRecord, AgentState, CAPABILITY_HANDOFF, Snapshot, terminal_safe};
use crate::scanner::Scanner;
use crate::tmux::Tmux;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HANDOFF_VERSION: u32 = 1;
/// Maximum pasted text, header included. Agent input lines are not a bulk
/// transfer channel; longer context belongs in the repository.
const TEXT_LIMIT: usize = 32 * 1024;
const REQUEST_LIMIT: u64 = 64 * 1024;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
const SENT_LIST_LIMIT: usize = 50;
const SENT_RETENTION_LIMIT: usize = 200;
/// clap reports an unknown subcommand with this status, which is how an
/// older peer binary without `remote-handoff` answers.
const USAGE_EXIT_STATUS: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum HandoffKind {
    Explore,
    Implement,
    Review,
    Deploy,
}

impl HandoffKind {
    fn label(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Implement => "implement",
            Self::Review => "review",
            Self::Deploy => "deploy",
        }
    }
}

/// Snapshot filters shared by `find` and `handoff send`. Every filter must
/// match. Names compare case-insensitively, the working directory matches
/// whole path components, and the title is a case-insensitive substring.
#[derive(Debug, Default, Clone, clap::Args)]
pub struct TargetFilters {
    /// Configured machine alias, or the local host name.
    #[arg(long)]
    pub machine: Option<String>,
    /// Provider name such as codex or claude.
    #[arg(long)]
    pub provider: Option<String>,
    /// Attention state: blocked, done, working, idle, or unknown.
    #[arg(long)]
    pub state: Option<String>,
    /// tmux session name.
    #[arg(long)]
    pub session: Option<String>,
    /// Working directory, or a run of its path components such as
    /// `hypertectonic/tmux-agent`.
    #[arg(long)]
    pub cwd: Option<String>,
    /// Substring of the displayed title.
    #[arg(long)]
    pub title: Option<String>,
}

impl TargetFilters {
    fn matches(&self, agent: &AgentRecord) -> bool {
        let equals = |filter: &Option<String>, value: &str| {
            filter
                .as_deref()
                .is_none_or(|filter| filter.eq_ignore_ascii_case(value))
        };
        equals(&self.machine, &agent.host)
            && equals(&self.provider, &agent.agent)
            && equals(
                &self.state,
                &format!("{:?}", agent.attention).to_ascii_lowercase(),
            )
            && equals(&self.session, &agent.session_name)
            && self
                .cwd
                .as_deref()
                .is_none_or(|filter| path_components_match(filter, &agent.cwd))
            && self.title.as_deref().is_none_or(|filter| {
                agent
                    .title
                    .to_ascii_lowercase()
                    .contains(&filter.to_ascii_lowercase())
            })
    }
}

/// True when the filter's path components appear as one contiguous run of
/// the directory's components. `agent` does not match `tmux-agent`, while
/// `hypertectonic/tmux-agent` matches `/home/agent/hypertectonic/tmux-agent`.
fn path_components_match(filter: &str, directory: &str) -> bool {
    let wanted = filter
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let actual = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if wanted.is_empty() {
        return false;
    }
    actual.windows(wanted.len()).any(|window| window == wanted)
}

/// Top-level agents that satisfy every filter. Subagent records share their
/// parent's pane and are never handoff targets, so they are excluded here
/// rather than turning a pane ID into an ambiguous match.
pub fn filter_agents<'a>(snapshot: &'a Snapshot, filters: &TargetFilters) -> Vec<&'a AgentRecord> {
    snapshot
        .agents
        .iter()
        .filter(|agent| agent.subagent.is_none() && filters.matches(agent))
        .collect()
}

/// Resolve exactly one top-level agent. `target` accepts a full ID, an
/// unambiguous ID suffix, or a pane ID. Bare pane IDs are local unless a
/// machine filter explicitly scopes them; other filters narrow further.
pub fn resolve_target<'a>(
    snapshot: &'a Snapshot,
    target: Option<&str>,
    filters: &TargetFilters,
) -> Result<&'a AgentRecord> {
    let matches = filter_agents(snapshot, filters)
        .into_iter()
        .filter(|agent| {
            target.is_none_or(|target| {
                if target.starts_with('%') {
                    return agent.pane_id == target
                        && (filters.machine.is_some() || agent.remote_alias.is_none());
                }
                agent.id == target
                    || agent.id.ends_with(target)
                    || format!("{}:{}", agent.host, agent.pane_id) == target
            })
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [record] => Ok(record),
        [] => bail!("no agent matches the requested target"),
        many => bail!(
            "target is ambiguous: {} agents match; narrow the filters or use a full ID ({})",
            many.len(),
            many.iter()
                .map(|agent| terminal_safe(&agent.id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn print_agents(agents: &[&AgentRecord], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(agents)?);
        return Ok(());
    }
    if agents.is_empty() {
        println!("No agents match.");
        return Ok(());
    }
    for agent in agents {
        println!(
            "{}\t{}\t{}@{}\t{}\t{}",
            terminal_safe(&agent.id),
            format!("{:?}", agent.attention).to_ascii_lowercase(),
            terminal_safe(&agent.agent),
            terminal_safe(&agent.host),
            terminal_safe(&agent.location_label()),
            terminal_safe(&agent.title)
        );
    }
    Ok(())
}

/// Typed request sent to the owning machine. It names the tmux server, the
/// server and session lifetimes, the pane, the pane's foreground process
/// group, and the provider. The receiver checks every field against its own
/// live scan and never interprets names or shell text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffRequest {
    version: u32,
    handoff_id: String,
    /// The tmux server name the sender's snapshot reported for the record.
    server: String,
    server_pid: u32,
    server_started_at: u64,
    session_created_at: u64,
    session_id: String,
    window_id: String,
    pane_id: String,
    pane_pid: u32,
    process_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_started_at_ms: Option<u64>,
    agent: String,
    allow_working: bool,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum HandoffResponse {
    Delivered {
        handoff_id: String,
        duplicate: bool,
    },
    Rejected {
        message: String,
    },
    /// Terminal input may have happened. This ID must never be pasted again.
    Uncertain {
        handoff_id: String,
        message: String,
    },
    /// The receiver runs a binary with a different handoff operation version.
    Unsupported {
        supported_version: u32,
    },
}

impl HandoffRequest {
    /// Build the request from a snapshot record. Older peers report no
    /// process identity and vanished sessions report zero lifetimes; both
    /// fail closed here rather than on the receiver.
    fn for_record(
        handoff_id: &str,
        record: &AgentRecord,
        allow_working: bool,
        text: String,
    ) -> Result<Self> {
        let session = record
            .session_connections
            .as_ref()
            .filter(|session| {
                session.server_pid != 0
                    && session.server_started_at != 0
                    && session.session_created_at != 0
            })
            .context(
                "target reports no live tmux server and session identity; refresh and retry",
            )?;
        let process = record.process.context(
            "target reports no agent process identity; update the owning tmux-agent binary",
        )?;
        let request = Self {
            version: HANDOFF_VERSION,
            handoff_id: handoff_id.to_string(),
            server: record.server.clone(),
            server_pid: session.server_pid,
            server_started_at: session.server_started_at,
            session_created_at: session.session_created_at,
            session_id: record.session_id.clone(),
            window_id: record.window_id.clone(),
            pane_id: record.pane_id.clone(),
            pane_pid: record.pane_pid,
            process_pid: process.pid,
            process_started_at_ms: process.started_at_ms,
            agent: record.agent.clone(),
            allow_working,
            text,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<()> {
        if self.version != HANDOFF_VERSION {
            bail!("unsupported handoff operation version {}", self.version);
        }
        validate_handoff_id(&self.handoff_id)?;
        validate_field("server", &self.server)?;
        for (value, prefix) in [
            (&self.session_id, '$'),
            (&self.window_id, '@'),
            (&self.pane_id, '%'),
        ] {
            if !value.starts_with(prefix)
                || value.len() < 2
                || !value[1..].bytes().all(|byte| byte.is_ascii_digit())
            {
                bail!("handoff requires numeric tmux session, window and pane IDs");
            }
        }
        if self.server_pid == 0 || self.server_started_at == 0 || self.session_created_at == 0 {
            bail!("handoff requires tmux server and session lifetime identity");
        }
        if self.pane_pid == 0 || self.process_pid == 0 {
            bail!("handoff requires the target pane and agent process identity");
        }
        if self
            .process_started_at_ms
            .is_none_or(|started| started == 0)
        {
            bail!("handoff requires the agent process start identity");
        }
        if self.agent.is_empty() || !self.agent.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            bail!("handoff requires an alphanumeric provider name");
        }
        validate_text(&self.text)
    }
}

fn validate_handoff_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("handoff IDs use 1 to 64 ASCII letters, digits, or hyphens");
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("handoff text is empty");
    }
    if text.len() > TEXT_LIMIT {
        bail!(
            "handoff text is {} bytes; the limit is {TEXT_LIMIT}",
            text.len()
        );
    }
    // Newlines and tabs are the only control characters an input line may
    // carry. Anything else could become an escape sequence in the target.
    if text
        .chars()
        .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        bail!("handoff text contains control characters other than newline and tab");
    }
    Ok(())
}

fn validate_field(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("handoff {name} is empty");
    }
    if value.chars().any(char::is_control) {
        bail!("handoff {name} contains control characters");
    }
    Ok(())
}

pub struct SendOptions {
    pub kind: HandoffKind,
    pub repository: String,
    pub branch: String,
    pub reference: Option<String>,
    pub from: String,
    pub allow_working: bool,
    pub handoff_id: Option<String>,
    pub body: String,
}

/// The header is the recipient's provenance: it is the first line the agent
/// reads and the line a human can grep for in either transcript.
fn compose_text(handoff_id: &str, options: &SendOptions) -> Result<String> {
    validate_field("repository", &options.repository)?;
    validate_field("branch", &options.branch)?;
    validate_field("sender", &options.from)?;
    if let Some(reference) = &options.reference {
        validate_field("reference", reference)?;
    }
    if options.body.trim().is_empty() {
        bail!("handoff body is empty");
    }
    let mut header = format!(
        "[tmux-agent handoff {handoff_id}] from={} kind={} repo={} branch={}",
        options.from.trim(),
        options.kind.label(),
        options.repository.trim(),
        options.branch.trim()
    );
    if let Some(reference) = &options.reference {
        header.push_str(&format!(" ref={}", reference.trim()));
    }
    let text = format!("{header}\n{}", options.body.trim_end_matches(['\n', '\r']));
    validate_text(&text)?;
    Ok(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SentStatus {
    /// The record was written and the control exchange has not finished.
    Sending,
    Delivered,
    /// The receiver had already delivered this handoff ID; nothing was pasted.
    Duplicate,
    /// The receiver refused before pasting anything.
    Failed,
    /// The transport failed after the request left; the receiver may or may
    /// not have pasted. A retry with the same ID queries the retained claim;
    /// an interrupted terminal submission remains uncertain.
    Unconfirmed,
    /// The peer binary does not speak this handoff operation version.
    Incompatible,
}

impl SentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Delivered => "delivered",
            Self::Duplicate => "duplicate",
            Self::Failed => "failed",
            Self::Unconfirmed => "unconfirmed",
            Self::Incompatible => "incompatible",
        }
    }
}

/// Sender-side record of one handoff. It is the only trace on the sending
/// machine; the recipient's transcript holds the same text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentRecord {
    pub handoff_id: String,
    pub sent_at_ms: u64,
    pub target_id: String,
    pub machine: String,
    pub pane_id: String,
    pub agent: String,
    pub kind: HandoffKind,
    pub repository: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub from: String,
    pub status: SentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub text: String,
    #[serde(default)]
    pub fingerprint: String,
}

pub struct SendReport {
    pub record: SentRecord,
    /// Delivery finished but the local sent record could not be updated.
    pub audit_warning: Option<String>,
}

#[derive(Debug)]
enum Route<'a> {
    Local,
    Machine(&'a MachineConfig),
}

/// Decide how a resolved record is reached, before any state is written.
fn route<'a>(config: &'a Config, snapshot: &Snapshot, record: &AgentRecord) -> Result<Route<'a>> {
    if !record.is_tmux() {
        bail!("handoff requires a tmux pane; ordinary terminal sessions have no delivery target");
    }
    if record.subagent.is_some() {
        bail!("handoff targets top-level agents; select the parent session");
    }
    let Some(alias) = record.remote_alias.as_deref() else {
        return Ok(Route::Local);
    };
    let Some(machine) = config.machine(alias) else {
        bail!(
            "remote handoff requires a structured [[machine]] configuration for {}; raw [[remote]] collectors define no control channel",
            terminal_safe(alias)
        );
    };
    let advertised = snapshot.peers.iter().any(|peer| {
        peer.name == alias
            && peer
                .capabilities
                .iter()
                .any(|capability| capability == CAPABILITY_HANDOFF)
    });
    if !advertised {
        bail!(
            "peer {} does not advertise {CAPABILITY_HANDOFF}; update the remote binary",
            terminal_safe(alias)
        );
    }
    Ok(Route::Machine(machine))
}

pub async fn send(
    tmux: &Tmux,
    config: &Config,
    paths: &RuntimePaths,
    snapshot: &Snapshot,
    record: &AgentRecord,
    options: SendOptions,
) -> Result<SendReport> {
    let route = route(config, snapshot, record)?;
    let handoff_id = match &options.handoff_id {
        Some(id) => {
            validate_handoff_id(id)?;
            id.clone()
        }
        None => generate_handoff_id()?,
    };
    let text = compose_text(&handoff_id, &options)?;
    let request = HandoffRequest::for_record(&handoff_id, record, options.allow_working, text)?;
    // The receiver owns tmux selection.  Never send the hub's local socket
    // arguments to a remote machine; its daemon was started with the target
    // server configuration.
    let mut sent = SentRecord {
        handoff_id,
        sent_at_ms: now_ms(),
        target_id: record.id.clone(),
        machine: record.host.clone(),
        pane_id: record.pane_id.clone(),
        agent: record.agent.clone(),
        kind: options.kind,
        repository: options.repository.trim().to_string(),
        branch: options.branch.trim().to_string(),
        reference: options
            .reference
            .as_deref()
            .map(|value| value.trim().to_string()),
        from: options.from.trim().to_string(),
        status: SentStatus::Sending,
        message: None,
        text: request.text.clone(),
        fingerprint: recipient_fingerprint(&request),
    };
    // The sender audit and receiver ledger use separate locks so local
    // delivery cannot recursively acquire its own flock.
    let root = ensure_dir(&paths.handoffs)?;
    let _audit_lock = FileLock::acquire(&root.join("sent.lock"))?;
    let prior_path = root
        .join("sent")
        .join(format!("{}.json", request.handoff_id));
    if prior_path.exists() {
        let prior: SentRecord = serde_json::from_slice(&fs::read(prior_path)?)?;
        if prior.fingerprint != sent.fingerprint || prior.machine != sent.machine {
            bail!("handoff ID was already used for a different recipient or message");
        }
        sent.sent_at_ms = prior.sent_at_ms;
    }
    write_sent(paths, &sent)?;
    let outcome = match route {
        // A local rejection carries the same typed shape as a remote one.
        Route::Local => Ok(
            deliver(tmux, config, paths, &request).unwrap_or_else(|error| {
                HandoffResponse::Rejected {
                    message: format!("{error:#}"),
                }
            }),
        ),
        Route::Machine(machine) => send_control(machine, &request).await,
    };
    let (status, message) = classify_outcome(&request, outcome);
    sent.status = status;
    sent.message = message;
    Ok(finish_report(paths, sent))
}

fn finish_report(paths: &RuntimePaths, sent: SentRecord) -> SendReport {
    let audit_warning = write_sent(paths, &sent).err().map(|error| {
        format!(
            "handoff {} {}, but its sent record could not be updated: {error:#}",
            sent.handoff_id,
            sent.status.label()
        )
    });
    SendReport {
        record: sent,
        audit_warning,
    }
}

/// Map the control exchange to a sender status. Only an explicit receiver
/// rejection means nothing was pasted; a transport error after the request
/// left is unconfirmed until a retry with the same ID answers.
fn classify_outcome(
    request: &HandoffRequest,
    outcome: Result<HandoffResponse>,
) -> (SentStatus, Option<String>) {
    match outcome {
        Ok(HandoffResponse::Delivered { handoff_id, .. }) if handoff_id != request.handoff_id => (
            SentStatus::Unconfirmed,
            Some("peer confirmed a different handoff ID; retry with the same --handoff-id".into()),
        ),
        Ok(HandoffResponse::Delivered {
            duplicate: true, ..
        }) => (SentStatus::Duplicate, None),
        Ok(HandoffResponse::Delivered { .. }) => (SentStatus::Delivered, None),
        Ok(HandoffResponse::Rejected { message }) => {
            (SentStatus::Failed, Some(terminal_safe(&message)))
        }
        Ok(HandoffResponse::Uncertain { message, .. }) => {
            (SentStatus::Unconfirmed, Some(terminal_safe(&message)))
        }
        Ok(HandoffResponse::Unsupported { supported_version }) => (
            SentStatus::Incompatible,
            Some(format!(
                "peer supports handoff operation version {supported_version}, this binary sends version {HANDOFF_VERSION}; update both machines together"
            )),
        ),
        Err(error) => match error.downcast_ref::<ControlExit>() {
            Some(ControlExit(Some(USAGE_EXIT_STATUS))) => (
                SentStatus::Incompatible,
                Some(
                    "peer binary has no remote-handoff operation; update the remote binary".into(),
                ),
            ),
            _ => (
                SentStatus::Unconfirmed,
                Some(format!(
                    "{}; retry with the same --handoff-id, the receiver answers duplicate if it was delivered",
                    terminal_safe(&format!("{error:#}"))
                )),
            ),
        },
    }
}

async fn send_control(
    machine: &MachineConfig,
    request: &HandoffRequest,
) -> Result<HandoffResponse> {
    let payload = serde_json::to_vec(request)?;
    let response = crate::focus::control_output(
        &machine.handoff_command(),
        &payload,
        REQUEST_LIMIT,
        DELIVERY_TIMEOUT,
    )
    .await?;
    serde_json::from_slice(&response).context("invalid remote handoff response")
}

/// Receiving side of the SSH control command. Always answers with a typed
/// response on stdout so the sender can record the exact outcome.
pub fn serve(tmux: &Tmux, config: &Config, paths: &RuntimePaths) -> Result<()> {
    let result = (|| {
        let mut input = Vec::new();
        std::io::stdin()
            .take(REQUEST_LIMIT + 1)
            .read_to_end(&mut input)?;
        if input.len() as u64 > REQUEST_LIMIT {
            bail!("handoff request exceeded size limit");
        }
        // Read only the version first so a newer sender gets a typed answer
        // instead of an unknown-field rejection.
        let version: VersionOnly =
            serde_json::from_slice(&input).context("invalid handoff request")?;
        if version.version != HANDOFF_VERSION {
            return Ok(HandoffResponse::Unsupported {
                supported_version: HANDOFF_VERSION,
            });
        }
        let request: HandoffRequest =
            serde_json::from_slice(&input).context("invalid handoff request")?;
        deliver(tmux, config, paths, &request)
    })();
    let response = match result {
        Ok(response) => response,
        Err(error) => HandoffResponse::Rejected {
            message: format!("{error:#}"),
        },
    };
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

fn deliver(
    tmux: &Tmux,
    config: &Config,
    paths: &RuntimePaths,
    request: &HandoffRequest,
) -> Result<HandoffResponse> {
    deliver_with(tmux, paths, request, || scan_once(tmux, config, paths))
}

/// One account-wide lock serializes claims and terminal submission, including
/// different tmux servers. This also serializes all senders to a given pane.
/// Pending claims survive crashes and can never be mistaken for delivery.
fn deliver_with(
    tmux: &Tmux,
    paths: &RuntimePaths,
    request: &HandoffRequest,
    scan: impl FnOnce() -> Result<Snapshot>,
) -> Result<HandoffResponse> {
    deliver_with_submission(tmux, paths, request, scan, submit_paste)
}

fn deliver_with_submission(
    tmux: &Tmux,
    paths: &RuntimePaths,
    request: &HandoffRequest,
    scan: impl FnOnce() -> Result<Snapshot>,
    submit: impl FnOnce(&Tmux, &HandoffRequest, &str) -> Result<()>,
) -> Result<HandoffResponse> {
    request.validate()?;
    let root = ensure_dir(&paths.handoffs)?;
    let ledger = ensure_dir(&root.join("claims"))?;
    let _lock = FileLock::acquire(&root.join("delivery.lock"))?;
    let marker = ledger.join(&request.handoff_id);
    if marker.exists() {
        let prior: Claim = serde_json::from_slice(&fs::read(&marker)?)?;
        let current = recipient_fingerprint(request);
        if prior.fingerprint != current {
            bail!("handoff ID was already used for a different recipient or message");
        }
        return Ok(match prior.state {
            ClaimState::Delivered => HandoffResponse::Delivered {
                handoff_id: request.handoff_id.clone(),
                duplicate: true,
            },
            ClaimState::Pending => uncertain(
                request,
                "previous submission was interrupted; inspect the recipient transcript before any new send",
            ),
        });
    }
    let snapshot = scan()?;
    validate_against_snapshot(&snapshot, request)?;
    let live = tmux
        .list_panes()?
        .into_iter()
        .find(|pane| pane.pane_id == request.pane_id && !pane.dead)
        .context("target pane vanished before delivery")?;
    if live.pane_pid != request.pane_pid {
        bail!("target pane was recreated before delivery");
    }
    // In copy mode, paste can reach the PTY while Enter is consumed by tmux.
    // Refuse before claiming or staging so leaving the mode permits a safe retry.
    let mode = tmux.run(&[
        "display-message",
        "-p",
        "-t",
        &request.pane_id,
        "#{pane_in_mode}",
    ])?;
    match mode.trim() {
        "0" => {}
        "1" => bail!("target pane is in copy mode or another tmux mode; leave it before retrying"),
        _ => bail!("target pane mode is unavailable; refusing to paste blindly"),
    }
    let mut claim = Claim {
        fingerprint: recipient_fingerprint(request),
        state: ClaimState::Pending,
    };
    atomic_json(&marker, &claim)?;
    let prepared = prepare_paste(tmux, &root, request);
    let buffer = match prepared {
        Ok(buffer) => buffer,
        Err(error) => {
            // Staging/loading never writes terminal input.
            fs::remove_file(&marker)?;
            return Err(error);
        }
    };
    if let Err(error) = submit(tmux, request, &buffer) {
        return Ok(uncertain(
            request,
            &format!(
                "terminal submission may be partial: {error:#}; inspect the recipient transcript"
            ),
        ));
    }
    claim.state = ClaimState::Delivered;
    if let Err(error) = atomic_json(&marker, &claim) {
        return Ok(uncertain(
            request,
            &format!("terminal submission completed but delivery bookkeeping failed: {error:#}"),
        ));
    }
    Ok(HandoffResponse::Delivered {
        handoff_id: request.handoff_id.clone(),
        duplicate: false,
    })
}

fn recipient_fingerprint(request: &HandoffRequest) -> String {
    // Typed serialization includes every identity field and the exact text.
    // The working-state override is not part of message/recipient identity.
    let mut identity = request.clone();
    identity.allow_working = false;
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&identity).expect("serializable request"))
    )
}

#[derive(Serialize, Deserialize)]
struct Claim {
    fingerprint: String,
    state: ClaimState,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaimState {
    Pending,
    Delivered,
}

fn uncertain(request: &HandoffRequest, message: &str) -> HandoffResponse {
    HandoffResponse::Uncertain {
        handoff_id: request.handoff_id.clone(),
        message: message.into(),
    }
}

/// Stage text and load a unique tmux buffer without touching terminal input.
/// These failures are safe to retry, so the caller can release the claim.
fn prepare_paste(tmux: &Tmux, root: &Path, request: &HandoffRequest) -> Result<String> {
    let attempt = generate_handoff_id()?;
    let buffer = format!("tmux-agent-handoff-{attempt}");
    let staged = root.join(format!("{attempt}.txt"));
    fs::write(&staged, request.text.as_bytes())
        .with_context(|| format!("stage handoff text {}", staged.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))?;
    let staged_path = staged.to_string_lossy().into_owned();
    let loaded = tmux.run(&["load-buffer", "-b", &buffer, &staged_path]);
    let _ = fs::remove_file(&staged);
    loaded?;
    Ok(buffer)
}

fn submit_paste(tmux: &Tmux, request: &HandoffRequest, buffer: &str) -> Result<()> {
    // One tmux command sequence: the paste and Enter are queued together on
    // the server, and the buffer is deleted by the paste itself.
    if let Err(error) = tmux.run(&[
        "paste-buffer",
        "-p",
        "-d",
        "-b",
        buffer,
        "-t",
        &request.pane_id,
        ";",
        "send-keys",
        "-t",
        &request.pane_id,
        "Enter",
    ]) {
        let _ = tmux.run(&["delete-buffer", "-b", buffer]);
        return Err(error);
    }
    Ok(())
}

fn scan_once(tmux: &Tmux, config: &Config, paths: &RuntimePaths) -> Result<Snapshot> {
    let discovered_server_key = tmux.server_key()?;
    let tmux_server_observed = discovered_server_key.is_some();
    let server_key = discovered_server_key.unwrap_or_else(|| tmux.runtime_key());
    let persisted = crate::store::load(&paths.state).unwrap_or_default();
    let mut scanner = Scanner::new(
        config,
        tmux.clone(),
        &server_key,
        paths.runners.clone(),
        persisted,
        tmux_server_observed,
    )?;
    scanner.scan()
}

/// The receiver's own scan is the authority on what runs in the pane now.
/// The sender's snapshot may be a second old plus the SSH round trip.
fn validate_against_snapshot(snapshot: &Snapshot, request: &HandoffRequest) -> Result<()> {
    if snapshot.server != request.server {
        bail!(
            "configured SSH control targets tmux server {}, not {}; configure the same server for watch and remote-handoff",
            terminal_safe(&snapshot.server),
            terminal_safe(&request.server)
        );
    }
    let record = snapshot
        .agents
        .iter()
        .find(|agent| {
            agent.is_tmux()
                && agent.remote_alias.is_none()
                && agent.subagent.is_none()
                && agent.pane_id == request.pane_id
        })
        .context("no agent is detected in the target pane; it may have exited or been replaced")?;
    let session = record
        .session_connections
        .as_ref()
        .context("target session lifetime is unavailable")?;
    if session.server_pid != request.server_pid
        || session.server_started_at != request.server_started_at
    {
        bail!("target belongs to a different or restarted tmux server");
    }
    if record.session_id != request.session_id
        || session.session_created_at != request.session_created_at
    {
        bail!("target session was replaced or the pane changed sessions");
    }
    if record.window_id != request.window_id {
        bail!("target pane moved to another window");
    }
    if record.pane_pid != request.pane_pid {
        bail!("target pane was recreated since the sender looked it up");
    }
    let process = record
        .process
        .context("target agent process identity is unavailable")?;
    if process.pid != request.process_pid {
        bail!("target agent process was restarted or replaced since the sender looked it up");
    }
    match (process.started_at_ms, request.process_started_at_ms) {
        (Some(actual), Some(expected)) if actual == expected => {}
        _ => bail!("target agent process start identity is unavailable or changed"),
    }
    if !record.agent.eq_ignore_ascii_case(&request.agent) {
        bail!(
            "target pane now runs {} rather than {}",
            terminal_safe(&record.agent),
            terminal_safe(&request.agent)
        );
    }
    match record.state {
        AgentState::Blocked => {
            bail!("target is blocked on a prompt; refusing to type into a pending prompt")
        }
        AgentState::Unknown => bail!("target state is unknown; refusing to paste blindly"),
        AgentState::Working if !request.allow_working => {
            bail!("target is working; retry when it is idle or pass --allow-working")
        }
        AgentState::Working | AgentState::Idle => Ok(()),
    }
}

/// Serializes account-wide delivery or sender audit work until dropped.
struct FileLock {
    _file: fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("open handoff lock {}", path.display()))?;
        #[cfg(unix)]
        {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                bail!(
                    "lock handoff file {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                );
            }
        }
        Ok(Self { _file: file })
    }
}

fn ensure_dir(path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure {}", path.display()))?;
    Ok(path.to_path_buf())
}

fn sent_dir(paths: &RuntimePaths) -> Result<PathBuf> {
    ensure_dir(&paths.handoffs)?;
    ensure_dir(&paths.handoffs.join("sent"))
}

fn write_sent(paths: &RuntimePaths, record: &SentRecord) -> Result<()> {
    let directory = sent_dir(paths)?;
    let path = directory.join(format!("{}.json", record.handoff_id));
    atomic_json(&path, record)?;
    let mut records = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
    records.retain(|entry| entry.path().extension().is_some_and(|ext| ext == "json"));
    records.sort_by_key(|entry| {
        std::cmp::Reverse(entry.metadata().and_then(|meta| meta.modified()).ok())
    });
    for entry in records.into_iter().skip(SENT_RETENTION_LIMIT) {
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("state file has no parent")?;
    let temporary = parent.join(format!("{}.tmp", generate_handoff_id()?));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    let result = (|| {
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn load_sent(paths: &RuntimePaths) -> Result<Vec<SentRecord>> {
    let directory = paths.handoffs.join("sent");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut records = fs::read_dir(&directory)
        .with_context(|| format!("read {}", directory.display()))?
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let bytes = fs::read(entry.path()).ok()?;
            serde_json::from_slice::<SentRecord>(&bytes).ok()
        })
        .collect::<Vec<_>>();
    records.sort_by_key(|record| std::cmp::Reverse(record.sent_at_ms));
    Ok(records)
}

pub fn print_sent(records: &[SentRecord], json: bool) -> Result<()> {
    let records = &records[..records.len().min(SENT_LIST_LIMIT)];
    if json {
        println!("{}", serde_json::to_string_pretty(records)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("No handoffs sent.");
        return Ok(());
    }
    for record in records {
        let detail = record
            .message
            .as_deref()
            .map(|message| format!(" ({message})"))
            .unwrap_or_default();
        println!(
            "{}\t{}\t{}\t{}\t{}@{}{}",
            record.handoff_id,
            record.status.label(),
            terminal_safe(&record.target_id),
            record.kind.label(),
            terminal_safe(&record.repository),
            terminal_safe(&record.branch),
            terminal_safe(&detail)
        );
    }
    Ok(())
}

fn generate_handoff_id() -> Result<String> {
    let mut bytes = [0_u8; 8];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("read random handoff ID")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AgentOrigin, AgentState, Attention, ClientConnection, MoshEndpoint, PeerStatus,
        ProcessIdentity, SessionConnections, SubagentInfo,
    };
    use crate::tmux::Pane;
    use std::process::Command;
    use std::time::Instant;

    fn session() -> SessionConnections {
        SessionConnections {
            server_pid: 10,
            server_started_at: 20,
            session_created_at: 30,
            complete: true,
            clients: Vec::new(),
        }
    }

    fn record(id: &str, pane: &str) -> AgentRecord {
        AgentRecord {
            id: id.into(),
            host: "local".into(),
            server: "default".into(),
            pane_id: pane.into(),
            pane_pid: 4242,
            process: Some(ProcessIdentity {
                pid: 4300,
                started_at_ms: Some(1_000),
            }),
            session_id: "$1".into(),
            session_name: "work".into(),
            window_id: "@2".into(),
            window_index: 1,
            window_name: "codex".into(),
            pane_index: 0,
            agent: "codex".into(),
            state: AgentState::Idle,
            attention: Attention::Idle,
            source: Default::default(),
            title: "fix parser".into(),
            label: None,
            cwd: "/home/agent/hypertectonic/tmux-agent".into(),
            visible: true,
            seen: true,
            changed_at_ms: 1,
            origin: AgentOrigin::Tmux,
            terminal: None,
            remote_alias: None,
            ssh_connection: None,
            session_connections: Some(session()),
            focus_target: None,
            goal: None,
            subagent: None,
            detection: None,
        }
    }

    fn remote_record(alias: &str, pane: &str) -> AgentRecord {
        let mut record = record(&format!("remote/{alias}/{alias}/default/{pane}"), pane);
        record.host = alias.into();
        record.remote_alias = Some(alias.into());
        record
    }

    fn snapshot(agents: Vec<AgentRecord>, peers: Vec<PeerStatus>) -> Snapshot {
        Snapshot {
            server: "default".into(),
            agents,
            peers,
            ..Snapshot::default()
        }
    }

    fn peer(name: &str, capabilities: &[&str]) -> PeerStatus {
        PeerStatus {
            name: name.into(),
            connected: true,
            last_error: None,
            application_version: Some("0.9.0".into()),
            protocol: crate::model::PROTOCOL_VERSION,
            capabilities: capabilities.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn machine(name: &str) -> MachineConfig {
        MachineConfig {
            name: name.into(),
            host: format!("{name}.example.ts.net"),
            ssh_user: "agent".into(),
            binary: "/home/agent/.local/bin/tmux-agent".into(),
            config: None,
            auto_connect: true,
        }
    }

    fn request(text: &str) -> HandoffRequest {
        HandoffRequest::for_record(
            "abc-123",
            &record("local/default/%3", "%3"),
            false,
            text.into(),
        )
        .unwrap()
    }

    fn options() -> SendOptions {
        SendOptions {
            kind: HandoffKind::Review,
            repository: "hypertectonic/tmux-agent".into(),
            branch: "feature/agent-handoff".into(),
            reference: Some("#144".into()),
            from: "local/default/%1".into(),
            allow_working: false,
            handoff_id: None,
            body: "Review the parser change.\nRun cargo test.\n\n".into(),
        }
    }

    #[test]
    fn request_validation_rejects_bad_ids_versions_lifetimes_and_control_text() {
        request("hello").validate().unwrap();
        let mut stale = request("hello");
        stale.version = 2;
        assert!(stale.validate().is_err());
        for (field, value) in [
            ("session", "work"),
            ("window", "@1; kill-server"),
            ("pane", "%"),
            ("id", "../escape"),
            ("id", ""),
            ("agent", "co dex"),
            ("server", ""),
            ("server", "default\n"),
        ] {
            let mut bad = request("hello");
            match field {
                "session" => bad.session_id = value.into(),
                "window" => bad.window_id = value.into(),
                "pane" => bad.pane_id = value.into(),
                "id" => bad.handoff_id = value.into(),
                "agent" => bad.agent = value.into(),
                "server" => bad.server = value.into(),
                _ => unreachable!(),
            }
            assert!(bad.validate().is_err(), "accepted {field} {value:?}");
        }
        for field in ["pane", "process", "server", "started", "created"] {
            let mut zero = request("hello");
            match field {
                "pane" => zero.pane_pid = 0,
                "process" => zero.process_pid = 0,
                "server" => zero.server_pid = 0,
                "started" => zero.server_started_at = 0,
                "created" => zero.session_created_at = 0,
                _ => unreachable!(),
            }
            assert!(zero.validate().is_err(), "accepted zero {field}");
        }
        let mut control = request("hello");
        control.text = "line\x1b[2J".into();
        assert!(control.validate().is_err());
        assert!(request("tabs\tand\nnewlines are fine").validate().is_ok());
        let mut oversized = request("hello");
        oversized.text = "x".repeat(TEXT_LIMIT + 1);
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn request_construction_fails_closed_without_lifetime_or_process_identity() {
        let mut vanished = record("local/default/%3", "%3");
        vanished.session_connections = Some(SessionConnections {
            server_pid: 0,
            server_started_at: 0,
            session_created_at: 0,
            complete: true,
            clients: Vec::new(),
        });
        assert!(HandoffRequest::for_record("id", &vanished, false, "x".into()).is_err());
        let mut no_session = record("local/default/%3", "%3");
        no_session.session_connections = None;
        assert!(HandoffRequest::for_record("id", &no_session, false, "x".into()).is_err());
        let mut older_peer = record("local/default/%3", "%3");
        older_peer.process = None;
        let error = HandoffRequest::for_record("id", &older_peer, false, "x".into()).unwrap_err();
        assert!(error.to_string().contains("update"), "{error}");
        let full = request("x");
        assert_eq!(
            (
                full.server.as_str(),
                full.server_pid,
                full.server_started_at,
                full.session_created_at
            ),
            ("default", 10, 20, 30)
        );
        assert_eq!(
            (full.process_pid, full.process_started_at_ms),
            (4300, Some(1_000))
        );
    }

    #[test]
    fn composed_text_carries_provenance_header_and_preserves_multiline_body() {
        let text = compose_text("abc-123", &options()).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap(),
            "[tmux-agent handoff abc-123] from=local/default/%1 kind=review repo=hypertectonic/tmux-agent branch=feature/agent-handoff ref=#144"
        );
        assert_eq!(lines.next().unwrap(), "Review the parser change.");
        assert_eq!(lines.next().unwrap(), "Run cargo test.");
        assert!(
            lines.next().is_none(),
            "trailing newlines must not submit empty turns"
        );
        let mut bad = options();
        bad.branch = "main\n; rm".into();
        assert!(compose_text("abc-123", &bad).is_err());
        let mut empty = options();
        empty.body = "\n".into();
        assert!(compose_text("abc-123", &empty).is_err());
    }

    #[test]
    fn target_resolution_requires_exactly_one_top_level_match() {
        let mut other = record("local/default/%2", "%2");
        other.agent = "claude".into();
        other.cwd = "/home/agent/other".into();
        other.attention = Attention::Working;
        // A Codex child shares its parent's pane and must not make the pane
        // ID ambiguous.
        let mut child = record("local/default/%1/thread-9", "%1");
        child.subagent = Some(SubagentInfo {
            parent_id: "local/default/%1".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("worker".into()),
            thread_id: Some("thread-9".into()),
        });
        let snapshot = snapshot(vec![record("local/default/%1", "%1"), other, child], vec![]);
        let filters = TargetFilters::default();
        assert_eq!(filter_agents(&snapshot, &filters).len(), 2);
        assert!(resolve_target(&snapshot, None, &filters).is_err());
        assert_eq!(
            resolve_target(&snapshot, Some("%1"), &filters).unwrap().id,
            "local/default/%1"
        );
        assert!(resolve_target(&snapshot, Some("thread-9"), &filters).is_err());
        let by_provider = TargetFilters {
            provider: Some("Claude".into()),
            ..TargetFilters::default()
        };
        assert_eq!(
            resolve_target(&snapshot, None, &by_provider)
                .unwrap()
                .pane_id,
            "%2"
        );
        let by_cwd_and_state = TargetFilters {
            cwd: Some("other".into()),
            state: Some("working".into()),
            ..TargetFilters::default()
        };
        assert_eq!(filter_agents(&snapshot, &by_cwd_and_state).len(), 1);
        let none = TargetFilters {
            machine: Some("thinkcat".into()),
            ..TargetFilters::default()
        };
        assert!(resolve_target(&snapshot, None, &none).is_err());
        let conflicting = TargetFilters {
            provider: Some("codex".into()),
            ..TargetFilters::default()
        };
        assert!(resolve_target(&snapshot, Some("%2"), &conflicting).is_err());
    }

    #[test]
    fn bare_pane_targets_are_local_unless_a_machine_is_explicit() {
        let local = record("local/default/%3", "%3");
        let remote = remote_record("build-host", "%3");
        let candidates = snapshot(vec![local.clone(), remote.clone()], vec![]);
        let filters = TargetFilters::default();
        assert_eq!(
            resolve_target(&candidates, Some("%3"), &filters)
                .unwrap()
                .id,
            local.id
        );
        let remote_only = snapshot(vec![remote.clone()], vec![]);
        assert!(resolve_target(&remote_only, Some("%3"), &filters).is_err());
        let explicit = TargetFilters {
            machine: Some("build-host".into()),
            ..TargetFilters::default()
        };
        assert_eq!(
            resolve_target(&candidates, Some("%3"), &explicit)
                .unwrap()
                .id,
            remote.id
        );
        for target in [remote.id.as_str(), "build-host:%3", "build-host/default/%3"] {
            assert_eq!(
                resolve_target(&candidates, Some(target), &filters)
                    .unwrap()
                    .id,
                remote.id
            );
        }
        // Host text is not proof of locality; the federation marker is.
        let mut same_host_remote = remote;
        same_host_remote.host = "local".into();
        assert!(
            resolve_target(
                &snapshot(vec![same_host_remote], vec![]),
                Some("%3"),
                &filters,
            )
            .is_err()
        );
    }

    #[test]
    fn pane_target_scoping_preserves_ambiguity_checks() {
        let filters = TargetFilters::default();
        let local_collision = snapshot(
            vec![record("local/one/%3", "%3"), record("local/two/%3", "%3")],
            vec![],
        );
        assert!(
            resolve_target(&local_collision, Some("%3"), &filters)
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        let remote = remote_record("build-host", "%3");
        let mut other_server = remote.clone();
        other_server.id = "remote/build-host/build-host/other/%3".into();
        other_server.server = "other".into();
        let remote_collision = snapshot(vec![remote.clone(), other_server], vec![]);
        let explicit = TargetFilters {
            machine: Some("build-host".into()),
            ..TargetFilters::default()
        };
        for (target, filters) in [("%3", &explicit), ("build-host:%3", &filters)] {
            assert!(
                resolve_target(&remote_collision, Some(target), filters)
                    .unwrap_err()
                    .to_string()
                    .contains("ambiguous")
            );
        }
        assert_eq!(
            resolve_target(&remote_collision, Some(&remote.id), &filters)
                .unwrap()
                .id,
            remote.id
        );
    }

    #[test]
    fn cwd_filter_matches_whole_path_components_only() {
        let directory = "/home/agent/hypertectonic/tmux-agent";
        for filter in [
            "tmux-agent",
            "hypertectonic/tmux-agent",
            "/home/agent",
            "agent/hypertectonic",
            "/home/agent/hypertectonic/tmux-agent",
            "/home/agent/hypertectonic/tmux-agent/",
        ] {
            assert!(path_components_match(filter, directory), "{filter}");
        }
        for filter in ["mux-agent", "home/hypertectonic", "tmux-agent/src", "", "/"] {
            assert!(!path_components_match(filter, directory), "{filter}");
        }
    }

    #[test]
    fn routing_uses_the_configured_machine_and_its_advertised_capability() {
        let config = Config {
            machines: vec![machine("build-host")],
            ..Config::default()
        };
        let local = record("local/default/%1", "%1");
        let advertised = snapshot(vec![], vec![peer("build-host", &[CAPABILITY_HANDOFF])]);
        assert!(matches!(
            route(&config, &advertised, &local).unwrap(),
            Route::Local
        ));

        let mut mosh_transport = remote_record("build-host", "%7");
        mosh_transport.session_connections = Some(SessionConnections {
            clients: vec![ClientConnection::Mosh {
                endpoint: MoshEndpoint {
                    address: "127.0.0.1".into(),
                    port: 60001,
                },
            }],
            ..session()
        });
        // The visible transport may be Mosh; control still uses configured SSH.
        match route(&config, &advertised, &mosh_transport).unwrap() {
            Route::Machine(machine) => {
                let command = machine.handoff_command();
                assert_eq!(command[0], "ssh");
                assert!(command.contains(&"BatchMode=yes".to_string()));
                let operation = command.last().unwrap();
                assert!(operation.ends_with(" remote-handoff"), "{operation}");
                assert!(operation.contains("/home/agent/.local/bin/tmux-agent"));
            }
            Route::Local => panic!("remote record routed locally"),
        }

        let older_peer = snapshot(vec![], vec![peer("build-host", &["remote_tmux_focus_v1"])]);
        let error = route(&config, &older_peer, &mosh_transport).unwrap_err();
        assert!(error.to_string().contains(CAPABILITY_HANDOFF));

        let unconfigured = remote_record("raw-collector", "%7");
        let error = route(&config, &advertised, &unconfigured).unwrap_err();
        assert!(error.to_string().contains("[[machine]]"));

        let mut terminal = local.clone();
        terminal.origin = AgentOrigin::Terminal;
        assert!(route(&config, &advertised, &terminal).is_err());
        let mut child = local.clone();
        child.subagent = Some(SubagentInfo {
            parent_id: "local/default/%1".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: None,
            thread_id: None,
        });
        assert!(route(&config, &advertised, &child).is_err());
    }

    #[test]
    fn outcomes_distinguish_rejection_transport_loss_and_incompatibility() {
        let request = request("hello");
        let delivered = classify_outcome(
            &request,
            Ok(HandoffResponse::Delivered {
                handoff_id: "abc-123".into(),
                duplicate: false,
            }),
        );
        assert_eq!(delivered.0, SentStatus::Delivered);
        let duplicate = classify_outcome(
            &request,
            Ok(HandoffResponse::Delivered {
                handoff_id: "abc-123".into(),
                duplicate: true,
            }),
        );
        assert_eq!(duplicate.0, SentStatus::Duplicate);
        let other_id = classify_outcome(
            &request,
            Ok(HandoffResponse::Delivered {
                handoff_id: "zzz".into(),
                duplicate: false,
            }),
        );
        assert_eq!(other_id.0, SentStatus::Unconfirmed);
        let rejected = classify_outcome(
            &request,
            Ok(HandoffResponse::Rejected {
                message: "target is blocked\x1b".into(),
            }),
        );
        assert_eq!(rejected.0, SentStatus::Failed);
        assert_eq!(rejected.1.as_deref(), Some("target is blocked "));
        let unsupported = classify_outcome(
            &request,
            Ok(HandoffResponse::Unsupported {
                supported_version: 7,
            }),
        );
        assert_eq!(unsupported.0, SentStatus::Incompatible);
        assert!(unsupported.1.unwrap().contains("version 7"));
        let missing_command = classify_outcome(&request, Err(ControlExit(Some(2)).into()));
        assert_eq!(missing_command.0, SentStatus::Incompatible);
        let ssh_failure = classify_outcome(&request, Err(ControlExit(Some(255)).into()));
        assert_eq!(ssh_failure.0, SentStatus::Unconfirmed);
        assert!(ssh_failure.1.unwrap().contains("--handoff-id"));
        let timeout = classify_outcome(&request, Err(anyhow::anyhow!("SSH control timed out")));
        assert_eq!(timeout.0, SentStatus::Unconfirmed);
    }

    #[test]
    fn receiver_validation_rejects_other_servers_stale_replaced_and_busy_targets() {
        let live = record("host/default/%3", "%3");
        let live_snapshot = snapshot(vec![live.clone()], vec![]);
        let request = request("hello");
        validate_against_snapshot(&live_snapshot, &request).unwrap();

        let mut other_server = live_snapshot.clone();
        other_server.server = "inner".into();
        let error = validate_against_snapshot(&other_server, &request).unwrap_err();
        assert!(error.to_string().contains("remote-handoff"), "{error}");

        type Mutation = fn(&mut HandoffRequest);
        let cases: [(&str, Mutation); 8] = [
            ("recreated pane", |r| r.pane_pid = 4243),
            ("moved window", |r| r.window_id = "@9".into()),
            ("replaced session", |r| r.session_id = "$9".into()),
            ("older session", |r| r.session_created_at = 31),
            ("restarted server", |r| r.server_started_at = 21),
            ("other server pid", |r| r.server_pid = 11),
            ("restarted agent", |r| r.process_pid = 4301),
            ("reused agent pid", |r| {
                r.process_started_at_ms = Some(2_000)
            }),
        ];
        for (name, mutate) in cases {
            let mut stale = request.clone();
            mutate(&mut stale);
            assert!(
                validate_against_snapshot(&live_snapshot, &stale).is_err(),
                "accepted {name}"
            );
        }
        let mut vanished = request.clone();
        vanished.pane_id = "%99".into();
        assert!(validate_against_snapshot(&live_snapshot, &vanished).is_err());

        // Missing process start identity must fail closed.
        let mut no_start = live.clone();
        no_start.process = Some(ProcessIdentity {
            pid: 4300,
            started_at_ms: None,
        });
        assert!(validate_against_snapshot(&snapshot(vec![no_start], vec![]), &request).is_err());

        let mut other_provider = live.clone();
        other_provider.agent = "claude".into();
        assert!(
            validate_against_snapshot(&snapshot(vec![other_provider], vec![]), &request).is_err()
        );
        let mut remote_only = live.clone();
        remote_only.remote_alias = Some("elsewhere".into());
        assert!(validate_against_snapshot(&snapshot(vec![remote_only], vec![]), &request).is_err());

        for (state, allowed_when_forced) in [
            (AgentState::Blocked, false),
            (AgentState::Unknown, false),
            (AgentState::Working, true),
        ] {
            let mut busy = live.clone();
            busy.state = state;
            let busy_snapshot = snapshot(vec![busy], vec![]);
            assert!(validate_against_snapshot(&busy_snapshot, &request).is_err());
            let mut forced = request.clone();
            forced.allow_working = true;
            assert_eq!(
                validate_against_snapshot(&busy_snapshot, &forced).is_ok(),
                allowed_when_forced,
                "{state:?}"
            );
        }
    }

    #[test]
    fn sent_records_round_trip_and_list_newest_first() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        let mut first = SentRecord {
            handoff_id: "first".into(),
            sent_at_ms: 100,
            target_id: "remote/build-host/x".into(),
            machine: "build-host".into(),
            pane_id: "%1".into(),
            agent: "codex".into(),
            kind: HandoffKind::Implement,
            repository: "repo".into(),
            branch: "develop".into(),
            reference: None,
            from: "war-room".into(),
            status: SentStatus::Sending,
            message: None,
            text: "[header]\nbody".into(),
            fingerprint: "test-fingerprint".into(),
        };
        write_sent(&paths, &first).unwrap();
        first.status = SentStatus::Unconfirmed;
        first.message = Some("peer offline".into());
        write_sent(&paths, &first).unwrap();
        let second = SentRecord {
            handoff_id: "second".into(),
            sent_at_ms: 200,
            status: SentStatus::Delivered,
            ..first.clone()
        };
        write_sent(&paths, &second).unwrap();
        let records = load_sent(&paths).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.handoff_id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
        assert_eq!(records[1].status, SentStatus::Unconfirmed);
        assert_eq!(records[1].message.as_deref(), Some("peer offline"));
        #[cfg(unix)]
        {
            let mode = fs::metadata(paths.handoffs.join("sent/first.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Retention bounds actual full-text files, rather than only the CLI
        // listing. Receiver claims are separate and never evicted by this.
        for index in 0..SENT_RETENTION_LIMIT {
            first.handoff_id = format!("retained-{index}");
            first.sent_at_ms = index as u64;
            write_sent(&paths, &first).unwrap();
        }
        assert_eq!(
            fs::read_dir(paths.handoffs.join("sent")).unwrap().count(),
            SENT_RETENTION_LIMIT
        );
        let retained = load_sent(&paths).unwrap();
        assert_eq!(retained.len(), SENT_RETENTION_LIMIT);
        assert_eq!(
            retained
                .iter()
                .position(|record| record.handoff_id == "retained-0"),
            Some(SENT_RETENTION_LIMIT - 1)
        );
        let broken = test_paths(&directory.path().join("broken"));
        fs::write(&broken.handoffs, "not a directory").unwrap_err();
        fs::create_dir_all(directory.path().join("broken")).unwrap();
        fs::write(&broken.handoffs, "not a directory").unwrap();
        first.status = SentStatus::Delivered;
        let report = finish_report(&broken, first);
        assert_eq!(report.record.status, SentStatus::Delivered);
        assert!(report.audit_warning.is_some());
    }

    #[test]
    fn generated_ids_are_valid_and_unique() {
        let first = generate_handoff_id().unwrap();
        let second = generate_handoff_id().unwrap();
        validate_handoff_id(&first).unwrap();
        assert_ne!(first, second);
    }

    fn test_paths(root: &Path) -> RuntimePaths {
        RuntimePaths {
            socket: root.join("daemon.sock"),
            runners: root.join("runs"),
            state: root.join("state.json"),
            acknowledgements: root.join("acknowledged.json"),
            log: root.join("daemon.log"),
            handoffs: root.join("handoffs"),
        }
    }

    struct Server(String);

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-L", &self.0, "kill-server"])
                .output();
        }
    }

    /// An isolated tmux server whose only pane runs `cat` with its output
    /// discarded: the terminal echo shows each submitted line exactly once.
    /// The returned request carries the pane's real identity and a synthetic
    /// agent process identity; `snapshot_for` builds the matching scan result.
    fn cat_server(label: &str) -> Option<(Server, Tmux, HandoffRequest, Pane)> {
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("skipping {label}: tmux unavailable");
            return None;
        }
        let name = format!("handoff-{label}-{}", std::process::id());
        let server = Server(name.clone());
        let tmux = Tmux::new(&Config {
            tmux_args: vec!["-L".into(), name.clone()],
            ..Config::default()
        });
        tmux.run(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "target",
            "-x",
            "250",
            "-y",
            "40",
            "cat >/dev/null",
        ])
        .unwrap();
        let pane = tmux
            .list_panes()
            .unwrap()
            .into_iter()
            .find(|pane| pane.session_name == "target")
            .unwrap();
        let processes = tmux
            .fresh_process_snapshot(std::slice::from_ref(&pane))
            .unwrap();
        let session = tmux
            .session_connections(&processes)
            .unwrap()
            .remove(&pane.session_id)
            .unwrap();
        let request = HandoffRequest {
            server: name,
            server_pid: session.server_pid,
            server_started_at: session.server_started_at,
            session_created_at: session.session_created_at,
            session_id: pane.session_id.clone(),
            window_id: pane.window_id.clone(),
            pane_id: pane.pane_id.clone(),
            pane_pid: pane.pane_pid,
            process_pid: pane.pane_pid,
            process_started_at_ms: Some(1),
            ..request("placeholder")
        };
        Some((server, tmux, request, pane))
    }

    /// A scan result that agrees with the live pane, as the receiver's own
    /// scanner would report for a detected idle agent.
    fn snapshot_for(request: &HandoffRequest, pane: &Pane) -> Snapshot {
        let mut record = record("host/x/%0", &request.pane_id);
        record.server = request.server.clone();
        record.pane_pid = pane.pane_pid;
        record.process = Some(ProcessIdentity {
            pid: request.process_pid,
            started_at_ms: request.process_started_at_ms,
        });
        record.session_id = request.session_id.clone();
        record.window_id = request.window_id.clone();
        record.session_connections = Some(SessionConnections {
            server_pid: request.server_pid,
            server_started_at: request.server_started_at,
            session_created_at: request.session_created_at,
            complete: true,
            clients: Vec::new(),
        });
        Snapshot {
            server: request.server.clone(),
            agents: vec![record],
            ..Snapshot::default()
        }
    }

    fn capture(tmux: &Tmux, pane_id: &str) -> String {
        tmux.run(&["capture-pane", "-p", "-t", pane_id]).unwrap()
    }

    fn wait_for(tmux: &Tmux, pane_id: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let screen = capture(tmux, pane_id);
            if screen.contains(needle) {
                return screen;
            }
            assert!(Instant::now() < deadline, "pane never showed {needle:?}");
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    #[test]
    fn same_id_is_bound_to_content_and_recipient_even_across_servers() {
        let Some((_one, tmux, request, pane)) = cat_server("id-one") else {
            return;
        };
        let Some((_two, other_tmux, mut other, other_pane)) = cat_server("id-two") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        let live = snapshot_for(&request, &pane);
        deliver_with(&tmux, &paths, &request, || Ok(live.clone())).unwrap();
        let mut changed = request.clone();
        changed.text.push_str(" changed");
        assert!(deliver_with(&tmux, &paths, &changed, || panic!("conflict scans")).is_err());
        other.handoff_id = request.handoff_id.clone();
        let other_live = snapshot_for(&other, &other_pane);
        assert!(deliver_with(&other_tmux, &paths, &other, || Ok(other_live)).is_err());
        assert!(!capture(&other_tmux, &other.pane_id).contains("placeholder"));
    }

    #[test]
    fn pending_and_partial_submission_never_claim_delivery_or_repeat_input() {
        let Some((_server, tmux, mut request, pane)) = cat_server("partial") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        let ledger = ensure_dir(&paths.handoffs.join("claims")).unwrap();
        atomic_json(
            &ledger.join(&request.handoff_id),
            &Claim {
                fingerprint: recipient_fingerprint(&request),
                state: ClaimState::Pending,
            },
        )
        .unwrap();
        assert!(matches!(
            deliver_with(&tmux, &paths, &request, || panic!("pending scans")),
            Ok(HandoffResponse::Uncertain { .. })
        ));
        request.handoff_id = "partial-paste".into();
        request.text = "partial unique text".into();
        let live = snapshot_for(&request, &pane);
        let response = deliver_with_submission(
            &tmux,
            &paths,
            &request,
            || Ok(live),
            |tmux, request, buffer| {
                tmux.run(&[
                    "paste-buffer",
                    "-p",
                    "-d",
                    "-b",
                    buffer,
                    "-t",
                    &request.pane_id,
                ])?;
                bail!("injected failure after paste before Enter")
            },
        )
        .unwrap();
        assert!(matches!(response, HandoffResponse::Uncertain { .. }));
        wait_for(&tmux, &request.pane_id, "partial unique text");
        assert!(matches!(
            deliver_with(&tmux, &paths, &request, || panic!("pending scans")),
            Ok(HandoffResponse::Uncertain { .. })
        ));
        assert_eq!(
            capture(&tmux, &request.pane_id)
                .matches("partial unique text")
                .count(),
            1
        );
    }

    #[test]
    fn conflicting_concurrent_ids_have_exactly_one_winner() {
        let Some((_server, tmux, request, pane)) = cat_server("id-race") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        let live = snapshot_for(&request, &pane);
        let results = std::thread::scope(|scope| {
            let handles = (0..4)
                .map(|index| {
                    let tmux = &tmux;
                    let paths = &paths;
                    let live = live.clone();
                    let mut request = request.clone();
                    request.text = format!("race message {index}");
                    scope.spawn(move || deliver_with(tmux, paths, &request, || Ok(live)))
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let screen = wait_for(&tmux, &request.pane_id, "race message");
        assert_eq!(screen.matches("race message").count(), 1);
    }

    #[test]
    fn copy_mode_refuses_input_without_claiming_and_allows_same_id_retry() {
        let Some((_server, tmux, mut request, pane)) = cat_server("copy-mode") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        request.text = "copy mode guarded message".into();
        let live = snapshot_for(&request, &pane);
        tmux.run(&["copy-mode", "-t", &request.pane_id]).unwrap();
        let mode = || {
            tmux.run(&[
                "display-message",
                "-p",
                "-t",
                &request.pane_id,
                "#{pane_in_mode}",
            ])
            .unwrap()
        };
        assert_eq!(mode().trim(), "1");
        let error = deliver_with(&tmux, &paths, &request, || Ok(live.clone())).unwrap_err();
        assert!(error.to_string().contains("mode"), "{error}");
        assert_eq!(mode().trim(), "1", "delivery must not cancel copy mode");
        assert!(
            !paths
                .handoffs
                .join("claims")
                .join(&request.handoff_id)
                .exists()
        );
        tmux.run(&["send-keys", "-t", &request.pane_id, "-X", "cancel"])
            .unwrap();
        assert_eq!(mode().trim(), "0");
        // Inspect the underlying terminal after leaving its frozen copy-mode
        // view: a paste without Enter would still have been echoed by the PTY.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!capture(&tmux, &request.pane_id).contains(&request.text));
        assert_eq!(
            deliver_with(&tmux, &paths, &request, || Ok(live)).unwrap(),
            HandoffResponse::Delivered {
                handoff_id: request.handoff_id.clone(),
                duplicate: false,
            }
        );
        let screen = wait_for(&tmux, &request.pane_id, &request.text);
        assert_eq!(screen.matches(&request.text).count(), 1);
        assert!(matches!(
            deliver_with(&tmux, &paths, &request, || panic!("duplicate scanned")),
            Ok(HandoffResponse::Delivered {
                duplicate: true,
                ..
            })
        ));
    }

    #[test]
    fn delivery_submits_multiline_text_once_and_answers_a_retry_as_duplicate() {
        let Some((_server, tmux, mut request, pane)) = cat_server("multiline") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        request.text = compose_text(&request.handoff_id, &options()).unwrap();
        let live = snapshot_for(&request, &pane);
        let first = deliver_with(&tmux, &paths, &request, || Ok(live.clone())).unwrap();
        assert_eq!(
            first,
            HandoffResponse::Delivered {
                handoff_id: request.handoff_id.clone(),
                duplicate: false
            }
        );
        let screen = wait_for(&tmux, &request.pane_id, "Run cargo test.");
        for line in request.text.lines() {
            assert_eq!(
                screen.matches(line).count(),
                1,
                "line {line:?} in {screen:?}"
            );
        }
        // The ledger answers before any validation, so a retry whose target
        // has since changed is still reported as delivered, not rejected.
        let stale_retry = request.clone();
        let retry = deliver_with(&tmux, &paths, &stale_retry, || {
            panic!("a duplicate must be answered without scanning")
        })
        .unwrap();
        assert_eq!(
            retry,
            HandoffResponse::Delivered {
                handoff_id: request.handoff_id.clone(),
                duplicate: true
            }
        );
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            capture(&tmux, &request.pane_id)
                .matches("Run cargo test.")
                .count(),
            1
        );
        assert!(
            paths
                .handoffs
                .join("claims")
                .join(&request.handoff_id)
                .exists()
        );
        assert!(
            !fs::read_dir(&paths.handoffs)
                .unwrap()
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "txt")),
            "staged text must not remain on disk"
        );
    }

    #[test]
    fn delivery_refuses_stale_targets_without_typing_or_claiming_the_id() {
        let Some((_server, tmux, request, pane)) = cat_server("stale") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        let live = snapshot_for(&request, &pane);
        let mut recreated = request.clone();
        recreated.pane_pid += 1;
        recreated.text = "must not appear".into();
        assert!(deliver_with(&tmux, &paths, &recreated, || Ok(live.clone())).is_err());
        let mut restarted = request.clone();
        restarted.process_pid += 1;
        restarted.text = "must not appear".into();
        assert!(deliver_with(&tmux, &paths, &restarted, || Ok(live.clone())).is_err());
        let mut other_server = request.clone();
        other_server.server = "elsewhere".into();
        other_server.text = "must not appear".into();
        assert!(deliver_with(&tmux, &paths, &other_server, || Ok(live.clone())).is_err());
        let mut missing = request.clone();
        missing.pane_id = "%999".into();
        missing.text = "must not appear".into();
        assert!(deliver_with(&tmux, &paths, &missing, || Ok(live.clone())).is_err());
        // Validation passes but the pane vanished between scan and paste.
        let mut vanished = request.clone();
        vanished.text = "must not appear".into();
        let mut ghost = live.clone();
        ghost.agents[0].pane_id = "%999".into();
        vanished.pane_id = "%999".into();
        assert!(deliver_with(&tmux, &paths, &vanished, || Ok(ghost.clone())).is_err());
        std::thread::sleep(Duration::from_millis(200));
        assert!(!capture(&tmux, &request.pane_id).contains("must not appear"));
        let ledger = paths.handoffs.join("claims");
        assert!(
            fs::read_dir(&ledger)
                .map(|entries| entries.count() == 0)
                .unwrap_or(true)
        );
    }

    #[test]
    fn concurrent_deliveries_to_one_pane_never_interleave() {
        let Some((_server, tmux, request, pane)) = cat_server("concurrent") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let live = snapshot_for(&request, &pane);
        let senders = (0..4)
            .map(|index| {
                let tmux = tmux.clone();
                let paths = test_paths(directory.path());
                let live = live.clone();
                let mut request = request.clone();
                request.handoff_id = format!("sender-{index}");
                request.text = (0..3)
                    .map(|line| format!("sender{index} line{line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                std::thread::spawn(move || {
                    deliver_with(&tmux, &paths, &request, || Ok(live)).unwrap()
                })
            })
            .collect::<Vec<_>>();
        for sender in senders {
            assert!(matches!(
                sender.join().unwrap(),
                HandoffResponse::Delivered {
                    duplicate: false,
                    ..
                }
            ));
        }
        wait_for(&tmux, &request.pane_id, "sender3 line2");
        std::thread::sleep(Duration::from_millis(200));
        let screen = capture(&tmux, &request.pane_id);
        let lines = screen
            .lines()
            .filter(|line| line.starts_with("sender"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 12, "{screen}");
        for block in lines.chunks(3) {
            let sender = &block[0][..7];
            assert!(
                block.iter().all(|line| line.starts_with(sender)),
                "interleaved block {block:?}"
            );
        }
    }

    #[test]
    fn nested_inner_server_receives_text_while_attached_from_an_outer_server() {
        let Some((inner_server, inner, mut request, pane)) = cat_server("nested-inner") else {
            return;
        };
        let outer_name = format!("handoff-nested-outer-{}", std::process::id());
        let outer_server = Server(outer_name.clone());
        let outer = Tmux::new(&Config {
            tmux_args: vec!["-L".into(), outer_name],
            ..Config::default()
        });
        let attach = format!("tmux -L {} attach-session -t target", inner_server.0);
        outer
            .run(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "outer",
                "-x",
                "120",
                "-y",
                "40",
                &attach,
            ])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let clients = inner
                .run(&["list-clients", "-t", "target", "-F", "#{client_pid}"])
                .unwrap();
            if !clients.trim().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "outer client never attached");
            std::thread::sleep(Duration::from_millis(30));
        }
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        request.text = "nested one\nnested two".into();
        let live = snapshot_for(&request, &pane);
        // A scan of the outer server must not satisfy the inner request.
        let mut outer_scan = live.clone();
        outer_scan.server = outer_server.0.clone();
        assert!(deliver_with(&inner, &paths, &request, || Ok(outer_scan.clone())).is_err());
        deliver_with(&inner, &paths, &request, || Ok(live.clone())).unwrap();
        wait_for(&inner, &request.pane_id, "nested two");
        // The outer client mirrors the inner pane, so the text is visible there too.
        let outer_pane = outer
            .list_panes()
            .unwrap()
            .into_iter()
            .find(|pane| pane.session_name == "outer")
            .unwrap();
        wait_for(&outer, &outer_pane.pane_id, "nested two");
    }
}
