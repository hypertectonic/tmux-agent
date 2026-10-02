use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub const PROTOCOL_VERSION: u32 = 4;
pub const LAUNCHER_PROTOCOL_VERSION: u32 = 1;
pub const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CAPABILITY_SUBAGENT_VIEW: &str = "codex_subagent_view_v1";
pub const CAPABILITY_REMOTE_FOCUS: &str = "remote_tmux_focus_v1";
pub const CAPABILITY_HANDOFF: &str = "handoff_v1";
pub const SUBAGENT_VIEW_MINIMUM_VERSION: &str = "0.2.0";

pub fn application_capabilities() -> Vec<String> {
    vec![
        CAPABILITY_SUBAGENT_VIEW.to_string(),
        CAPABILITY_REMOTE_FOCUS.to_string(),
        CAPABILITY_HANDOFF.to_string(),
    ]
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Working,
    Blocked,
    Idle,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    Blocked,
    Done,
    Working,
    Idle,
    #[default]
    Unknown,
}

impl Attention {
    pub fn rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Done => 1,
            Self::Working => 2,
            Self::Idle => 3,
            Self::Unknown => 4,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Blocked => "!",
            Self::Done => "✓",
            Self::Working => "●",
            Self::Idle => "○",
            Self::Unknown => "?",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Screen,
    Process,
    Title,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOrigin {
    #[default]
    Tmux,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Pursuing,
    Achieved,
    Unmet,
}

impl GoalState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Achieved | Self::Unmet)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GoalProgress {
    Elapsed {
        #[serde(rename = "elapsed_seconds")]
        seconds: u64,
    },
    Tokens {
        #[serde(rename = "used_tokens")]
        used: u64,
        #[serde(
            rename = "budget_tokens",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        budget: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalInfo {
    pub state: GoalState,
    #[serde(flatten)]
    pub progress: GoalProgress,
    // Legacy wire names cover notification/acknowledgement of both terminal outcomes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub achievement_pending: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub achievement_observed_at_ms: u64,
}

// Old readers require elapsed_seconds in `goal` and know only pursuing/achieved.
// Additive siblings let them retain the agent or runner and ignore only its goal.
pub(crate) mod goal_wire {
    use super::{GoalInfo, GoalProgress, GoalState};
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    #[derive(Default, Serialize, Deserialize)]
    struct GoalWire {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<GoalInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_goal: Option<GoalInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unmet_goal: Option<UnmetGoal>,
    }

    // Keep this budgeted outcome out of both pre-unmet wire fields. Required
    // token fields also prevent an elapsed-only or budgetless unmet outcome.
    #[derive(Serialize, Deserialize)]
    struct UnmetGoal {
        state: GoalState,
        used_tokens: u64,
        budget_tokens: u64,
        #[serde(default, skip_serializing_if = "super::is_false")]
        achievement_pending: bool,
        #[serde(default, skip_serializing_if = "super::is_zero")]
        achievement_observed_at_ms: u64,
    }

    pub fn serialize<S: Serializer>(
        goal: &Option<GoalInfo>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut wire = GoalWire::default();
        if let Some(goal) = goal {
            match (goal.state, goal.progress) {
                (
                    GoalState::Unmet,
                    GoalProgress::Tokens {
                        used,
                        budget: Some(budget),
                    },
                ) => {
                    wire.unmet_goal = Some(UnmetGoal {
                        state: GoalState::Unmet,
                        used_tokens: used,
                        budget_tokens: budget,
                        achievement_pending: goal.achievement_pending,
                        achievement_observed_at_ms: goal.achievement_observed_at_ms,
                    });
                }
                (GoalState::Unmet, _) => {
                    return Err(serde::ser::Error::custom(
                        "unmet goal requires token usage and budget",
                    ));
                }
                (_, GoalProgress::Elapsed { .. }) => wire.goal = Some(*goal),
                (_, GoalProgress::Tokens { .. }) => wire.token_goal = Some(*goal),
            }
        }
        wire.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<GoalInfo>, D::Error> {
        let wire = GoalWire::deserialize(deserializer)?;
        match (wire.goal, wire.token_goal, wire.unmet_goal) {
            (None, None, None) => Ok(None),
            (Some(goal), None, None)
                if goal.state != GoalState::Unmet
                    && matches!(goal.progress, GoalProgress::Elapsed { .. }) =>
            {
                Ok(Some(goal))
            }
            (None, Some(goal), None)
                if goal.state != GoalState::Unmet
                    && matches!(goal.progress, GoalProgress::Tokens { .. }) =>
            {
                Ok(Some(goal))
            }
            (None, None, Some(goal)) if goal.state == GoalState::Unmet => Ok(Some(GoalInfo {
                state: GoalState::Unmet,
                progress: GoalProgress::Tokens {
                    used: goal.used_tokens,
                    budget: Some(goal.budget_tokens),
                },
                achievement_pending: goal.achievement_pending,
                achievement_observed_at_ms: goal.achievement_observed_at_ms,
            })),
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                Err(D::Error::custom("conflicting goal fields"))
            }
            _ => Err(D::Error::custom(
                "goal state or progress does not match its wire field",
            )),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentInfo {
    pub parent_id: String,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SshConnection {
    pub client_address: String,
    pub client_port: u16,
    pub server_address: String,
    pub server_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoshEndpoint {
    pub address: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum ClientConnection {
    Ssh { connection: SshConnection },
    Mosh { endpoint: MoshEndpoint },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConnections {
    pub server_pid: u32,
    pub server_started_at: u64,
    pub session_created_at: u64,
    /// False when an attached client's transport cannot be inspected.
    pub complete: bool,
    pub clients: Vec<ClientConnection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxTarget {
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub pane_id: String,
    pub pane_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTransport {
    pub connection: Option<SshConnection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mosh_endpoint: Option<MoshEndpoint>,
    pub remote_host: String,
    #[serde(default)]
    pub remote_host_explicit: bool,
    pub remote_session: Option<String>,
    pub title: String,
    pub label: Option<String>,
    #[serde(default)]
    pub visible: bool,
    pub target: TmuxTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionDetails {
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector: Option<String>,
    pub observed_state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub definitive: bool,
    #[serde(default)]
    pub inferred: bool,
    #[serde(default)]
    pub preserve_previous: bool,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
}

/// The foreground process group that carries the agent in its pane, with a
/// stable kernel start time. Custom persistent shell loops can replace a
/// provider while retaining this group, so this is not an execution identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub host: String,
    pub server: String,
    pub pane_id: String,
    pub pane_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessIdentity>,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_index: u32,
    pub agent: String,
    pub state: AgentState,
    pub attention: Attention,
    pub source: EvidenceSource,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub cwd: String,
    pub visible: bool,
    pub seen: bool,
    pub changed_at_ms: u64,
    #[serde(default)]
    pub origin: AgentOrigin,
    #[serde(default)]
    pub terminal: Option<String>,
    #[serde(default)]
    pub remote_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_connection: Option<SshConnection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_connections: Option<SessionConnections>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_target: Option<TmuxTarget>,
    #[serde(flatten, with = "goal_wire")]
    pub goal: Option<GoalInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionDetails>,
}

impl AgentRecord {
    pub fn is_tmux(&self) -> bool {
        self.origin == AgentOrigin::Tmux
    }

    pub fn location_label(&self) -> String {
        if let Some(target) = &self.focus_target {
            return format!(
                "{}:{}.{}",
                target.session_name, target.window_index, target.pane_index
            );
        }
        match self.origin {
            AgentOrigin::Tmux => {
                format!(
                    "{}:{}.{}",
                    self.session_name, self.window_index, self.pane_index
                )
            }
            AgentOrigin::Terminal => format!(
                "tty {}",
                self.terminal.as_deref().unwrap_or("unknown terminal")
            ),
        }
    }

    pub fn location(&self) -> String {
        format!("{}/{}", self.location_label(), self.host)
    }
}

pub fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
}

pub fn trim_braille_activity_prefix(value: &str) -> &str {
    let trimmed = value.trim_start();
    let mut characters = trimmed.char_indices();
    let Some((_, first)) = characters.next() else {
        return trimmed;
    };
    let Some((separator_index, separator)) = characters.next() else {
        return if is_codex_spinner(first) { "" } else { trimmed };
    };
    if ('\u{2800}'..='\u{28ff}').contains(&first) && separator.is_whitespace() {
        trimmed[separator_index..].trim()
    } else {
        trimmed.trim()
    }
}

fn is_codex_spinner(character: char) -> bool {
    matches!(
        character,
        '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇' | '⠏'
    )
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub connected: bool,
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_version: Option<String>,
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_version: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub revision: u64,
    pub host: String,
    pub server: String,
    pub generated_at_ms: u64,
    pub agents: Vec<AgentRecord>,
    #[serde(default)]
    pub peers: Vec<PeerStatus>,
    #[serde(skip)]
    pub ssh_transports: Vec<SshTransport>,
}

impl Snapshot {
    pub fn sort_agents(&mut self) {
        self.sort_agents_by_last_used(&HashMap::new());
    }

    pub(crate) fn sort_agents_by_last_used(&mut self, last_used_at_ms: &HashMap<String, u64>) {
        let known_ids = self
            .agents
            .iter()
            .map(|agent| agent.id.clone())
            .collect::<HashSet<_>>();
        let mut children = HashMap::<String, Vec<AgentRecord>>::new();
        let mut roots = Vec::new();
        for agent in std::mem::take(&mut self.agents) {
            let parent_id = agent
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_id.as_str())
                .filter(|parent_id| known_ids.contains(*parent_id));
            if let Some(parent_id) = parent_id {
                children
                    .entry(parent_id.to_string())
                    .or_default()
                    .push(agent);
            } else {
                roots.push(agent);
            }
        }
        roots.sort_by(|left, right| sort_agent(left, right, last_used_at_ms));
        for siblings in children.values_mut() {
            siblings.sort_by(sort_subagent);
        }
        let mut ordered = Vec::with_capacity(roots.len() + children.len());
        let mut visited = HashSet::new();
        for root in roots {
            append_agent_tree(root, &mut children, &mut visited, &mut ordered);
        }
        for remaining in children.into_values().flatten() {
            if visited.insert(remaining.id.clone()) {
                ordered.push(remaining);
            }
        }
        self.agents = ordered;
        self.peers.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

fn sort_agent(
    a: &AgentRecord,
    b: &AgentRecord,
    last_used_at_ms: &HashMap<String, u64>,
) -> Ordering {
    a.attention
        .rank()
        .cmp(&b.attention.rank())
        .then_with(|| {
            if a.attention == Attention::Idle && b.attention == Attention::Idle {
                idle_recency(b, last_used_at_ms).cmp(&idle_recency(a, last_used_at_ms))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| a.host.cmp(&b.host))
        .then_with(|| a.session_name.cmp(&b.session_name))
        .then_with(|| a.window_index.cmp(&b.window_index))
        .then_with(|| a.pane_index.cmp(&b.pane_index))
}

fn idle_recency(agent: &AgentRecord, last_used_at_ms: &HashMap<String, u64>) -> u64 {
    agent
        .changed_at_ms
        .max(last_used_at_ms.get(&agent.id).copied().unwrap_or_default())
}

fn sort_subagent(a: &AgentRecord, b: &AgentRecord) -> Ordering {
    let a_finished = a
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.finished_at_ms);
    let b_finished = b
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.finished_at_ms);
    a_finished
        .is_some()
        .cmp(&b_finished.is_some())
        .then_with(|| {
            a.subagent
                .as_ref()
                .map(|subagent| subagent.started_at_ms)
                .cmp(&b.subagent.as_ref().map(|subagent| subagent.started_at_ms))
        })
        .then_with(|| a.id.cmp(&b.id))
}

fn append_agent_tree(
    agent: AgentRecord,
    children: &mut HashMap<String, Vec<AgentRecord>>,
    visited: &mut HashSet<String>,
    ordered: &mut Vec<AgentRecord>,
) {
    if !visited.insert(agent.id.clone()) {
        return;
    }
    let id = agent.id.clone();
    ordered.push(agent);
    if let Some(descendants) = children.remove(&id) {
        for child in descendants {
            append_agent_tree(child, children, visited, ordered);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    Snapshot { local_only: bool },
    Watch { local_only: bool },
    Acknowledge { target: String },
    MarkAllRead,
    MarkUsed { target: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    Snapshot { snapshot: Snapshot },
    Ack,
    Acknowledged { count: usize },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub protocol: u32,
    pub host: String,
    pub server: String,
    pub agents: Vec<AgentRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcknowledgedState {
    pub protocol: u32,
    pub ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goal_achievements: Vec<GoalAcknowledgement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalAcknowledgement {
    pub id: String,
    pub achievement_observed_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sortable_agent(
        id: &str,
        session_name: &str,
        attention: Attention,
        changed_at_ms: u64,
    ) -> AgentRecord {
        AgentRecord {
            id: id.into(),
            host: "host".into(),
            server: "default".into(),
            pane_id: format!("%{id}"),
            pane_pid: 10,
            process: None,
            session_id: format!("${id}"),
            session_name: session_name.into(),
            window_id: format!("@{id}"),
            window_index: 1,
            window_name: "work".into(),
            pane_index: 0,
            agent: "Codex".into(),
            state: AgentState::Idle,
            attention,
            source: EvidenceSource::Screen,
            title: "work".into(),
            label: None,
            cwd: "/tmp".into(),
            visible: false,
            seen: true,
            changed_at_ms,
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

    #[test]
    fn last_used_reorders_only_idle_roots() {
        let parent_id = "idle-parent";
        let parent = sortable_agent(parent_id, "c-parent", Attention::Idle, 1);
        let mut newer_child = sortable_agent("child-newer", "z-child", Attention::Idle, 1);
        newer_child.subagent = Some(SubagentInfo {
            parent_id: parent_id.into(),
            started_at_ms: 20,
            finished_at_ms: None,
            name: None,
            thread_id: None,
        });
        let mut older_child = sortable_agent("child-older", "a-child", Attention::Idle, 1);
        older_child.subagent = Some(SubagentInfo {
            parent_id: parent_id.into(),
            started_at_ms: 10,
            finished_at_ms: None,
            name: None,
            thread_id: None,
        });
        let mut snapshot = Snapshot {
            agents: vec![
                sortable_agent("unknown-z", "z", Attention::Unknown, 1),
                sortable_agent("idle-a", "a-idle", Attention::Idle, 50),
                sortable_agent("working-z", "z", Attention::Working, 1),
                newer_child,
                sortable_agent("done-z", "z", Attention::Done, 1),
                sortable_agent("blocked-z", "z", Attention::Blocked, 1),
                parent,
                sortable_agent("idle-b", "b-idle", Attention::Idle, 1),
                sortable_agent("blocked-a", "a", Attention::Blocked, 1),
                sortable_agent("done-a", "a", Attention::Done, 1),
                sortable_agent("working-a", "a", Attention::Working, 1),
                older_child,
                sortable_agent("unknown-a", "a", Attention::Unknown, 1),
            ],
            ..Snapshot::default()
        };
        let last_used_at_ms = HashMap::from([
            ("idle-b".to_string(), 100),
            ("blocked-z".to_string(), 1_000),
            ("child-newer".to_string(), 1_000),
        ]);

        snapshot.sort_agents_by_last_used(&last_used_at_ms);

        assert_eq!(
            snapshot
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            [
                "blocked-a",
                "blocked-z",
                "done-a",
                "done-z",
                "working-a",
                "working-z",
                "idle-b",
                "idle-a",
                "idle-parent",
                "child-older",
                "child-newer",
                "unknown-a",
                "unknown-z",
            ]
        );
    }

    #[test]
    fn old_agent_records_default_to_tmux_origin() {
        let record: AgentRecord = serde_json::from_value(serde_json::json!({
            "id": "host/default/%1",
            "host": "host",
            "server": "default",
            "pane_id": "%1",
            "pane_pid": 10,
            "session_id": "$1",
            "session_name": "main",
            "window_id": "@1",
            "window_index": 1,
            "window_name": "work",
            "pane_index": 0,
            "agent": "Codex",
            "state": "unknown",
            "attention": "unknown",
            "source": "process",
            "title": "",
            "cwd": "",
            "visible": false,
            "seen": true,
            "changed_at_ms": 1
        }))
        .unwrap();
        assert_eq!(record.origin, AgentOrigin::Tmux);
        assert!(record.terminal.is_none());
        assert!(record.ssh_connection.is_none());
        assert!(record.focus_target.is_none());
        assert!(record.goal.is_none());
        assert!(record.subagent.is_none());
        assert!(record.detection.is_none());
    }

    #[test]
    fn unmet_goal_wire_preserves_pre_unmet_snapshot_readers() {
        #[derive(Deserialize)]
        #[serde(tag = "state", rename_all = "snake_case")]
        enum OldGoal {
            Pursuing,
            Achieved,
        }
        #[derive(Deserialize)]
        struct OldAgent {
            id: String,
            state: AgentState,
            goal: Option<OldGoal>,
            token_goal: Option<OldGoal>,
        }
        #[derive(Deserialize)]
        struct OldSnapshot {
            protocol: u32,
            agents: Vec<OldAgent>,
        }

        let mut agent = sortable_agent("1", "work", Attention::Idle, 1);
        let goal = GoalInfo {
            state: GoalState::Unmet,
            progress: GoalProgress::Tokens {
                used: 50_500,
                budget: Some(50_000),
            },
            achievement_pending: true,
            achievement_observed_at_ms: 123,
        };
        agent.goal = Some(goal);
        let snapshot = Snapshot {
            protocol: PROTOCOL_VERSION,
            agents: vec![agent],
            ..Snapshot::default()
        };
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert!(encoded["agents"][0].get("goal").is_none());
        assert!(encoded["agents"][0].get("token_goal").is_none());
        assert_eq!(
            encoded["agents"][0]["unmet_goal"],
            serde_json::json!({
                "state": "unmet", "used_tokens": 50_500, "budget_tokens": 50_000,
                "achievement_pending": true, "achievement_observed_at_ms": 123
            })
        );
        let old: OldSnapshot = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(old.protocol, PROTOCOL_VERSION);
        assert_eq!(old.agents.len(), 1);
        assert_eq!(old.agents[0].id, "1");
        assert_eq!(old.agents[0].state, AgentState::Idle);
        assert!(old.agents[0].goal.is_none());
        assert!(old.agents[0].token_goal.is_none());
        assert_eq!(
            serde_json::from_value::<Snapshot>(encoded).unwrap().agents[0].goal,
            Some(goal)
        );
    }

    #[test]
    fn unmet_goal_wire_rejects_wrong_state_progress_and_conflicts() {
        let agent = sortable_agent("1", "work", Attention::Idle, 1);
        let unmet =
            serde_json::json!({"state": "unmet", "used_tokens": 50_500, "budget_tokens": 50_000});
        for field in ["goal", "token_goal"] {
            let mut encoded = serde_json::to_value(&agent).unwrap();
            encoded[field] = unmet.clone();
            assert!(serde_json::from_value::<AgentRecord>(encoded.clone()).is_err());
            encoded[field] = if field == "goal" {
                serde_json::json!({"state": "pursuing", "elapsed_seconds": 42})
            } else {
                serde_json::json!({"state": "pursuing", "used_tokens": 40_000, "budget_tokens": 50_000})
            };
            encoded["unmet_goal"] = unmet.clone();
            assert!(serde_json::from_value::<AgentRecord>(encoded).is_err());
        }
        for invalid in [
            serde_json::json!({"state": "achieved", "used_tokens": 50_500, "budget_tokens": 50_000}),
            serde_json::json!({"state": "pursuing", "used_tokens": 50_500, "budget_tokens": 50_000}),
            serde_json::json!({"state": "unmet", "used_tokens": 50_500}),
            serde_json::json!({"state": "unmet", "elapsed_seconds": 42}),
        ] {
            let mut encoded = serde_json::to_value(&agent).unwrap();
            encoded["unmet_goal"] = invalid;
            assert!(serde_json::from_value::<AgentRecord>(encoded).is_err());
        }
        let mut encoded = serde_json::to_value(&agent).unwrap();
        encoded["unmet_goal"] = unmet.clone();
        encoded["unmet_goal"]["future_metadata"] = serde_json::json!(true);
        let restored = serde_json::from_value::<AgentRecord>(encoded).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap()["unmet_goal"], unmet);
        for progress in [
            GoalProgress::Elapsed { seconds: 42 },
            GoalProgress::Tokens {
                used: 50_000,
                budget: None,
            },
        ] {
            let mut invalid = agent.clone();
            invalid.goal = Some(GoalInfo {
                state: GoalState::Unmet,
                progress,
                achievement_pending: false,
                achievement_observed_at_ms: 0,
            });
            assert!(serde_json::to_value(invalid).is_err());
        }
    }

    #[test]
    fn token_goal_wire_preserves_legacy_snapshot_readers() {
        // These readers freeze the old required elapsed field and goal boundary.
        #[derive(Deserialize)]
        struct LegacyGoal {
            state: GoalState,
            elapsed_seconds: u64,
        }
        #[derive(Deserialize)]
        struct LegacyAgent {
            id: String,
            state: AgentState,
            goal: Option<LegacyGoal>,
        }
        #[derive(Deserialize)]
        struct LegacySnapshot {
            protocol: u32,
            agents: Vec<LegacyAgent>,
        }

        let mut agent = sortable_agent("1", "work", Attention::Idle, 1);
        let mut goal = GoalInfo {
            state: GoalState::Achieved,
            progress: GoalProgress::Tokens {
                used: 40_000,
                budget: None,
            },
            achievement_pending: true,
            achievement_observed_at_ms: 123,
        };
        agent.goal = Some(goal);
        let mut snapshot = Snapshot {
            protocol: PROTOCOL_VERSION,
            application_version: None,
            capabilities: vec![],
            revision: 1,
            host: "host".into(),
            server: "default".into(),
            generated_at_ms: 1,
            agents: vec![agent],
            peers: vec![],
            ssh_transports: vec![],
        };
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert!(encoded["agents"][0].get("goal").is_none());
        assert_eq!(
            encoded["agents"][0]["token_goal"],
            serde_json::json!({
                "state": "achieved", "used_tokens": 40_000,
                "achievement_pending": true, "achievement_observed_at_ms": 123
            })
        );
        let old: LegacySnapshot = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(old.protocol, 4);
        assert_eq!(old.agents.len(), 1);
        assert_eq!(old.agents[0].id, "1");
        assert_eq!(old.agents[0].state, AgentState::Idle);
        assert!(old.agents[0].goal.is_none());
        assert_eq!(
            serde_json::from_value::<Snapshot>(encoded).unwrap().agents[0].goal,
            Some(goal)
        );

        goal.progress = GoalProgress::Elapsed { seconds: 42 };
        snapshot.agents[0].goal = Some(goal);
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert!(encoded["agents"][0].get("token_goal").is_none());
        assert_eq!(
            encoded["agents"][0]["goal"],
            serde_json::json!({
                "state": "achieved", "elapsed_seconds": 42,
                "achievement_pending": true, "achievement_observed_at_ms": 123
            })
        );
        let old: LegacySnapshot = serde_json::from_value(encoded.clone()).unwrap();
        let old_goal = old.agents[0].goal.as_ref().unwrap();
        assert_eq!(old_goal.state, GoalState::Achieved);
        assert_eq!(old_goal.elapsed_seconds, 42);
        assert_eq!(
            serde_json::from_value::<Snapshot>(encoded).unwrap().agents[0].goal,
            Some(goal)
        );
    }

    #[test]
    fn goal_wire_rejects_conflicting_or_misnamed_progress() {
        let agent = sortable_agent("1", "work", Attention::Idle, 1);
        let elapsed = serde_json::json!({"state": "pursuing", "elapsed_seconds": 42});
        let tokens = serde_json::json!({"state": "pursuing", "used_tokens": 40_000, "budget_tokens": 50_000});
        for (goal, token_goal) in [
            (elapsed.clone(), tokens.clone()),
            (tokens.clone(), serde_json::Value::Null),
            (serde_json::Value::Null, elapsed),
        ] {
            let mut encoded = serde_json::to_value(&agent).unwrap();
            encoded["goal"] = goal;
            encoded["token_goal"] = token_goal;
            assert!(serde_json::from_value::<AgentRecord>(encoded).is_err());
        }
        let mut encoded = serde_json::to_value(&agent).unwrap();
        encoded["goal"] = serde_json::Value::Null;
        encoded["token_goal"] = tokens;
        let restored = serde_json::from_value::<AgentRecord>(encoded).unwrap();
        assert_eq!(
            restored.goal.unwrap().progress,
            GoalProgress::Tokens {
                used: 40_000,
                budget: Some(50_000)
            }
        );
    }

    #[test]
    fn old_goal_records_default_to_no_pending_achievement() {
        let goal: GoalInfo = serde_json::from_value(serde_json::json!({
            "state": "achieved",
            "elapsed_seconds": 7_920
        }))
        .unwrap();
        assert_eq!(goal.state, GoalState::Achieved);
        assert!(!goal.achievement_pending);
        assert_eq!(goal.achievement_observed_at_ms, 0);

        let encoded = serde_json::to_value(goal).unwrap();
        assert!(encoded.get("achievement_pending").is_none());
    }

    #[test]
    fn pending_goal_achievement_survives_serialization() {
        let goal = GoalInfo {
            state: GoalState::Achieved,
            progress: crate::model::GoalProgress::Elapsed { seconds: 7_920 },
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        };
        let encoded = serde_json::to_value(goal).unwrap();
        assert_eq!(encoded["achievement_pending"], true);
        assert_eq!(encoded["achievement_observed_at_ms"], 123_000);
        assert_eq!(serde_json::from_value::<GoalInfo>(encoded).unwrap(), goal);
    }

    #[test]
    fn old_acknowledgement_state_defaults_to_no_goal_events() {
        let state: AcknowledgedState = serde_json::from_value(serde_json::json!({
            "protocol": 2,
            "ids": ["host/default/%1"]
        }))
        .unwrap();

        assert_eq!(state.ids, ["host/default/%1"]);
        assert!(state.goal_achievements.is_empty());
    }

    #[test]
    fn old_subagent_metadata_defaults_to_no_thread_identity() {
        let subagent: SubagentInfo = serde_json::from_value(serde_json::json!({
            "parent_id": "host/default/%1",
            "started_at_ms": 10,
            "name": "review"
        }))
        .unwrap();

        assert!(subagent.thread_id.is_none());
    }

    #[test]
    fn old_snapshots_default_to_unknown_version_and_no_capabilities() {
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "revision": 2,
            "host": "host",
            "server": "default",
            "generated_at_ms": 3,
            "agents": []
        }))
        .unwrap();

        assert!(snapshot.application_version.is_none());
        assert!(snapshot.capabilities.is_empty());
    }

    #[test]
    fn local_transport_metadata_is_not_serialized_in_snapshots() {
        let snapshot = Snapshot {
            ssh_transports: vec![SshTransport {
                mosh_endpoint: None,
                connection: None,
                remote_host: "remote-mac".into(),
                remote_host_explicit: true,
                remote_session: Some("remote-session".into()),
                title: "project".into(),
                label: Some("private local label".into()),
                visible: false,
                target: TmuxTarget {
                    session_name: "local-session".into(),
                    window_id: "@1".into(),
                    window_index: 1,
                    pane_id: "%1".into(),
                    pane_index: 0,
                },
            }],
            ..Snapshot::default()
        };

        let encoded = serde_json::to_value(snapshot).unwrap();

        assert!(encoded.get("ssh_transports").is_none());
        assert!(!encoded.to_string().contains("private local label"));
    }

    #[test]
    fn old_peer_status_defaults_to_unknown_version_and_protocol() {
        let peer: PeerStatus = serde_json::from_value(serde_json::json!({
            "name": "remote-mac",
            "connected": true,
            "last_error": null,
        }))
        .unwrap();

        assert!(peer.application_version.is_none());
        assert_eq!(peer.protocol, 0);
        assert!(peer.capabilities.is_empty());
    }

    #[test]
    fn peer_status_has_no_false_freshness_timestamp() {
        let encoded = serde_json::to_value(PeerStatus {
            name: "build-host".into(),
            connected: true,
            last_error: None,
            application_version: Some(APPLICATION_VERSION.into()),
            protocol: PROTOCOL_VERSION,
            capabilities: application_capabilities(),
        })
        .unwrap();

        assert!(encoded.get("updated_at_ms").is_none());
        assert_eq!(encoded["connected"], true);
        assert_eq!(encoded["protocol"], PROTOCOL_VERSION);
    }

    #[test]
    fn codex_thread_metadata_contains_no_rollout_content() {
        let value = serde_json::to_value(SubagentInfo {
            parent_id: "host/default/%1".into(),
            started_at_ms: 10,
            finished_at_ms: None,
            name: Some("Worker".into()),
            thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
        })
        .unwrap();
        let fields = value.as_object().unwrap();

        assert_eq!(fields.len(), 4);
        assert!(fields.contains_key("parent_id"));
        assert!(fields.contains_key("started_at_ms"));
        assert!(fields.contains_key("name"));
        assert!(fields.contains_key("thread_id"));
    }

    #[test]
    fn detection_metadata_contains_only_derived_evidence() {
        let value = serde_json::to_value(DetectionDetails {
            engine: "provider".into(),
            detector: Some("Codex".into()),
            observed_state: AgentState::Blocked,
            signal: Some("confirmation_prompt".into()),
            scope: Some("after_prompt".into()),
            definitive: true,
            inferred: false,
            preserve_previous: false,
            transition: None,
        })
        .unwrap();
        let fields = value.as_object().unwrap();

        assert_eq!(fields.len(), 8);
        assert!(fields.contains_key("engine"));
        assert!(fields.contains_key("detector"));
        assert!(fields.contains_key("observed_state"));
        assert!(fields.contains_key("signal"));
        assert!(fields.contains_key("scope"));
        assert!(fields.contains_key("definitive"));
        assert!(fields.contains_key("inferred"));
        assert!(fields.contains_key("preserve_previous"));
        assert!(!fields.contains_key("screen"));
        assert!(!fields.contains_key("prompt"));
        assert!(!fields.contains_key("content"));
        assert!(!fields.contains_key("command"));
    }

    #[test]
    fn terminal_text_drops_control_characters() {
        assert_eq!(
            terminal_safe("safe\u{1b}]52;c;payload\u{7}\nnext"),
            "safe ]52;c;payload  next"
        );
    }

    #[test]
    fn trims_provider_braille_activity_prefixes_only() {
        assert_eq!(
            trim_braille_activity_prefix("⠦ sample-project"),
            "sample-project"
        );
        assert_eq!(trim_braille_activity_prefix("  ⠂  project  "), "project");
        assert_eq!(trim_braille_activity_prefix("⣿art"), "⣿art");
        assert_eq!(trim_braille_activity_prefix("⠹"), "");
        assert_eq!(trim_braille_activity_prefix("⣿"), "⣿");
        assert_eq!(trim_braille_activity_prefix("✳ project"), "✳ project");
        assert_eq!(trim_braille_activity_prefix("plain"), "plain");
    }
}
