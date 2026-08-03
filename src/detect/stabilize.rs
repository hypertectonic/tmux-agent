use super::Detection;
use crate::model::{AgentRecord, AgentState, EvidenceSource};
use std::collections::{HashMap, HashSet};

const INFERRED_IDLE_OBSERVATIONS: u8 = 2;

#[derive(Debug, Default)]
pub struct StateTracker {
    observations: HashMap<String, Observation>,
}

#[derive(Debug)]
struct Observation {
    identity: String,
    pending_idle: Option<PendingIdle>,
}

#[derive(Debug)]
struct PendingIdle {
    observations: u8,
}

impl StateTracker {
    pub fn stabilize(
        &mut self,
        id: &str,
        identity: &str,
        mut detection: Detection,
        previous: Option<&AgentRecord>,
        _now_ms: u64,
    ) -> Detection {
        let observation = self
            .observations
            .entry(id.to_string())
            .or_insert_with(|| Observation {
                identity: identity.to_string(),
                pending_idle: None,
            });

        let identity_changed = observation.identity != identity;
        if identity_changed {
            *observation = Observation {
                identity: identity.to_string(),
                pending_idle: None,
            };
        }

        let previous = (!identity_changed)
            .then_some(previous)
            .flatten()
            .filter(|record| record.agent == detection.agent);
        if detection
            .details
            .as_ref()
            .is_some_and(|details| details.preserve_previous)
        {
            observation.pending_idle = None;
            if detection.goal.is_none() {
                detection.goal = previous.and_then(|record| record.goal);
            }
            let reason = detection
                .details
                .as_ref()
                .and_then(|details| details.transition.clone())
                .unwrap_or_else(|| "preserved_previous_observation".into());
            let (state, source) = previous
                .map(|record| (record.state, record.source))
                .unwrap_or((AgentState::Idle, EvidenceSource::Process));
            hold(&mut detection, state, source, &reason);
            return detection;
        }

        let inferred_idle = detection.state == AgentState::Idle
            && detection
                .details
                .as_ref()
                .is_some_and(|details| details.inferred);
        let previous_was_active = previous.is_some_and(|record| {
            matches!(record.state, AgentState::Working | AgentState::Blocked)
        });

        if !inferred_idle || !previous_was_active {
            observation.pending_idle = None;
            return detection;
        }

        let pending = observation
            .pending_idle
            .get_or_insert(PendingIdle { observations: 0 });
        pending.observations = pending.observations.saturating_add(1);
        if pending.observations >= INFERRED_IDLE_OBSERVATIONS {
            observation.pending_idle = None;
            return detection;
        }

        let previous = previous.expect("active transition has a previous record");
        hold(
            &mut detection,
            previous.state,
            previous.source,
            "waiting_for_second_quiet_observation",
        );
        detection
    }

    pub fn retain(&mut self, active_ids: &HashSet<String>) {
        self.observations.retain(|id, _| active_ids.contains(id));
    }
}

fn hold(detection: &mut Detection, state: AgentState, source: EvidenceSource, reason: &str) {
    detection.state = state;
    detection.source = source;
    if let Some(details) = &mut detection.details {
        details.transition = Some(reason.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentOrigin, Attention, DetectionDetails, GoalInfo, GoalState};

    fn detection(
        state: AgentState,
        inferred: bool,
        definitive: bool,
        preserve_previous: bool,
    ) -> Detection {
        Detection {
            agent: "Codex".into(),
            state,
            source: EvidenceSource::Screen,
            goal: None,
            details: Some(DetectionDetails {
                engine: "provider".into(),
                detector: Some("Codex".into()),
                observed_state: state,
                signal: Some("test_signal".into()),
                scope: Some("test".into()),
                definitive,
                inferred,
                preserve_previous,
                transition: None,
            }),
        }
    }

    fn record(state: AgentState) -> AgentRecord {
        AgentRecord {
            id: "host/default/%1".into(),
            host: "host".into(),
            server: "default".into(),
            pane_id: "%1".into(),
            pane_pid: 1,
            session_id: "$1".into(),
            session_name: "main".into(),
            window_id: "@1".into(),
            window_index: 1,
            window_name: "work".into(),
            pane_index: 0,
            agent: "Codex".into(),
            state,
            attention: Attention::Working,
            source: EvidenceSource::Screen,
            title: String::new(),
            label: None,
            cwd: String::new(),
            visible: false,
            seen: true,
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

    #[test]
    fn new_agent_publishes_direct_working_evidence_immediately() {
        let mut tracker = StateTracker::default();
        let result = tracker.stabilize(
            "host/default/%1",
            "Codex:42",
            detection(AgentState::Working, false, true, false),
            None,
            1_000,
        );
        assert_eq!(result.state, AgentState::Working);
        assert!(result.details.unwrap().transition.is_none());
    }

    #[test]
    fn new_quiet_agent_starts_idle() {
        let mut tracker = StateTracker::default();
        let result = tracker.stabilize(
            "host/default/%1",
            "Codex:42",
            detection(AgentState::Idle, true, false, false),
            None,
            1_000,
        );
        assert_eq!(result.state, AgentState::Idle);
    }

    #[test]
    fn inferred_idle_requires_two_observations_after_activity() {
        let mut tracker = StateTracker::default();
        let old = record(AgentState::Working);
        let first = tracker.stabilize(
            "host/default/%1",
            "Codex:42",
            detection(AgentState::Idle, true, false, false),
            Some(&old),
            2_000,
        );
        assert_eq!(first.state, AgentState::Working);
        assert_eq!(
            first.details.unwrap().transition.as_deref(),
            Some("waiting_for_second_quiet_observation")
        );

        let second = tracker.stabilize(
            "host/default/%1",
            "Codex:42",
            detection(AgentState::Idle, true, false, false),
            Some(&old),
            2_500,
        );
        assert_eq!(second.state, AgentState::Idle);
    }

    #[test]
    fn direct_idle_evidence_clears_working_immediately() {
        let mut tracker = StateTracker::default();
        let old = record(AgentState::Working);
        let result = tracker.stabilize(
            "host/default/%1",
            "Codex:42",
            detection(AgentState::Idle, false, true, false),
            Some(&old),
            2_000,
        );
        assert_eq!(result.state, AgentState::Idle);
    }

    #[test]
    fn preserve_signal_keeps_previous_state_and_goal() {
        let mut tracker = StateTracker::default();
        let mut old = record(AgentState::Blocked);
        old.goal = Some(GoalInfo {
            state: GoalState::Pursuing,
            elapsed_seconds: 120,
            achievement_pending: false,
            achievement_observed_at_ms: 0,
        });
        let result = tracker.stabilize(
            "host/default/%1",
            "Codex:42",
            detection(AgentState::Unknown, false, false, true),
            Some(&old),
            2_000,
        );
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.goal, old.goal);
        assert_eq!(
            result.details.unwrap().transition.as_deref(),
            Some("preserved_previous_observation")
        );
    }

    #[test]
    fn detached_grok_quiet_redraw_remains_idle() {
        let mut tracker = StateTracker::default();
        let mut old = record(AgentState::Idle);
        old.agent = "Grok".into();
        let detection = crate::detect::detect_agent("Grok".into(), "\"task title\" - grok", "");
        assert_eq!(detection.state, AgentState::Idle);
        assert!(
            detection
                .details
                .as_ref()
                .is_some_and(|details| details.inferred)
        );

        let result = tracker.stabilize("host/default/%1", "Grok:42", detection, Some(&old), 2_000);

        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(result.source, EvidenceSource::Process);
    }

    #[test]
    fn grok_approval_prompt_interrupts_working_state() {
        let mut tracker = StateTracker::default();
        let mut old = record(AgentState::Working);
        old.agent = "Grok".into();
        let detection = crate::detect::detect_agent(
            "Grok".into(),
            "⠸ task - grok",
            "Allow this command?\n1. Yes\n2. No",
        );

        let result = tracker.stabilize("host/default/%1", "Grok:42", detection, Some(&old), 2_000);

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.source, EvidenceSource::Screen);
    }

    #[test]
    fn identity_change_does_not_inherit_previous_activity() {
        let mut tracker = StateTracker::default();
        let old = record(AgentState::Working);
        let quiet = || detection(AgentState::Idle, true, false, false);
        let first = tracker.stabilize("host/default/%1", "Codex:42", quiet(), Some(&old), 2_000);
        assert_eq!(first.state, AgentState::Working);

        let replaced = tracker.stabilize("host/default/%1", "Codex:99", quiet(), Some(&old), 2_500);
        assert_eq!(replaced.state, AgentState::Idle);
    }

    #[test]
    fn identity_change_does_not_preserve_previous_state() {
        let mut tracker = StateTracker::default();
        let old = record(AgentState::Working);
        let _ = tracker.stabilize(
            "host/default/%1",
            "Codex:42",
            detection(AgentState::Working, false, true, false),
            Some(&old),
            2_000,
        );

        let replaced = tracker.stabilize(
            "host/default/%1",
            "Codex:99",
            detection(AgentState::Unknown, false, false, true),
            Some(&old),
            2_500,
        );
        assert_eq!(replaced.state, AgentState::Idle);
    }
}
