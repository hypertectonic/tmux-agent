use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub const PROTOCOL_VERSION: u32 = 3;
pub const LAUNCHER_PROTOCOL_VERSION: u32 = 1;
pub const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CAPABILITY_SUBAGENT_VIEW: &str = "codex_subagent_view_v1";
pub const SUBAGENT_VIEW_MINIMUM_VERSION: &str = "0.2.0";

pub fn application_capabilities() -> Vec<String> {
    vec![CAPABILITY_SUBAGENT_VIEW.to_string()]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalInfo {
    pub state: GoalState,
    pub elapsed_seconds: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub achievement_pending: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub achievement_observed_at_ms: u64,
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
    pub remote_host: String,
    #[serde(default)]
    pub remote_host_explicit: bool,
    pub remote_session: Option<String>,
    pub title: String,
    pub label: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub host: String,
    pub server: String,
    pub pane_id: String,
    pub pane_pid: u32,
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
    pub focus_target: Option<TmuxTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
        self.agents.sort_by(sort_agent);
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

fn sort_agent(a: &AgentRecord, b: &AgentRecord) -> Ordering {
    a.attention
        .rank()
        .cmp(&b.attention.rank())
        .then_with(|| a.host.cmp(&b.host))
        .then_with(|| a.session_name.cmp(&b.session_name))
        .then_with(|| a.window_index.cmp(&b.window_index))
        .then_with(|| a.pane_index.cmp(&b.pane_index))
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
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    Snapshot { snapshot: Snapshot },
    Ack,
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
            elapsed_seconds: 7_920,
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
                connection: None,
                remote_host: "remote-mac".into(),
                remote_host_explicit: true,
                remote_session: Some("remote-session".into()),
                title: "project".into(),
                label: Some("private local label".into()),
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
