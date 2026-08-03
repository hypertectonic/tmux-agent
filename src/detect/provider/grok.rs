use super::ProviderDetection;
use super::screen::title_has_braille_activity;
use crate::model::AgentState;

pub(super) fn detect(title: &str, content: &str) -> ProviderDetection {
    let screen_observation = content.lines().rev().find_map(|line| {
        let line = normalize(line);
        if line.is_empty() {
            return None;
        }
        if contains_any(&line, BLOCKED_SIGNALS) {
            return Some(ProviderDetection::from_screen(
                AgentState::Blocked,
                "approval_prompt",
                "latest_visible_signal",
            ));
        }
        if contains_any(&line, WORKING_SIGNALS) {
            return Some(ProviderDetection::from_screen(
                AgentState::Working,
                "activity_marker",
                "latest_visible_signal",
            ));
        }
        if contains_any(&line, IDLE_SIGNALS) {
            return Some(ProviderDetection::from_screen(
                AgentState::Idle,
                "input_prompt",
                "latest_visible_signal",
            ));
        }
        None
    });

    if screen_observation.is_some_and(|observation| {
        matches!(observation.state, AgentState::Blocked | AgentState::Working)
    }) {
        return screen_observation.expect("active screen observation");
    }

    if let Some(observation) = title_observation(title) {
        return observation;
    }

    if let Some(observation) = screen_observation {
        return observation;
    }

    ProviderDetection::inferred_idle("grok_foreground_without_activity")
}

const BLOCKED_SIGNALS: &[&str] = &[
    "action required",
    "allow command?",
    "allow this command",
    "do you want to proceed?",
    "would you like to proceed?",
    "press enter to confirm",
    "enter to submit answer",
    "waiting for approval",
    "requires approval",
    "approve once",
    "always allow",
];

const WORKING_SIGNALS: &[&str] = &[
    "esc to interrupt",
    "ctrl+c to stop",
    "working (",
    "thinking (",
    "running command",
    "esc to cancel",
];

const IDLE_SIGNALS: &[&str] = &[
    "· ready ·",
    "goal achieved",
    "find and fix a bug in @filename",
    "shift+tab:mode",
    "type a message",
    "ask anything",
];

fn title_observation(title: &str) -> Option<ProviderDetection> {
    let title = normalize(title);
    if title.contains("action required") {
        Some(ProviderDetection::from_title(
            AgentState::Blocked,
            "title_requests_action",
        ))
    } else if title_has_braille_activity(&title)
        || title.contains(" working")
        || title.contains("thinking")
    {
        Some(ProviderDetection::from_title(
            AgentState::Working,
            "title_shows_activity",
        ))
    } else {
        None
    }
}

fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EvidenceSource;

    #[test]
    fn latest_input_prompt_outweighs_old_activity() {
        let result = detect("", "Working (3s)\nEsc to interrupt\nAsk anything");
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(result.signal, "input_prompt");
    }

    #[test]
    fn latest_activity_outweighs_old_input_prompt() {
        let result = detect("", "Ask anything\nThinking (3s)\nEsc to interrupt");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "activity_marker");
    }

    #[test]
    fn approval_prompt_is_blocked() {
        let result = detect("", "Allow this command?\n1. Yes\n2. No");
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "approval_prompt");
    }

    #[test]
    fn approval_prompt_outweighs_activity_title() {
        let result = detect("⠸ task - grok", "Allow this command?\n1. Yes\n2. No");
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.source, EvidenceSource::Screen);
        assert_eq!(result.signal, "approval_prompt");
    }

    #[test]
    fn escape_to_cancel_is_working() {
        let result = detect("\"task title\" - grok", "Running query\nEsc to cancel");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.source, EvidenceSource::Screen);
        assert_eq!(result.signal, "activity_marker");
    }

    #[test]
    fn static_floax_redraw_is_inferred_idle() {
        let result = detect("\"task title\" - grok", "");
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(result.source, EvidenceSource::Process);
        assert!(result.inferred);
    }
}
