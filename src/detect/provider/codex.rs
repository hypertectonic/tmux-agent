use super::ProviderDetection;
use super::screen::{Lines, VisibleScreen, codex_prompt, title_has_braille_activity};
use crate::model::AgentState;

pub(super) fn detect(title: &str, content: &str) -> ProviderDetection {
    if let Some(observation) = title_observation(title) {
        return observation;
    }

    let screen = VisibleScreen::new(content);
    let current_turn = screen.following_last(codex_prompt);
    if is_transcript_view(current_turn) {
        return ProviderDetection::preserve("transcript_view", "current_turn");
    }
    if requests_confirmation(current_turn) {
        return ProviderDetection::from_screen(
            AgentState::Blocked,
            "current_turn_requests_confirmation",
            "current_turn",
        );
    }
    if waits_for_choice(current_turn) {
        return ProviderDetection::from_screen(
            AgentState::Blocked,
            "current_turn_waits_for_choice",
            "current_turn",
        );
    }

    let recent = screen.recent_non_empty(4);
    if has_active_footer(recent) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "working_footer",
            "recent_lines",
        );
    }

    if !title.trim().is_empty() {
        return ProviderDetection::from_title(AgentState::Idle, "static_title");
    }

    ProviderDetection::inferred_idle("codex_foreground_without_activity")
}

fn title_observation(title: &str) -> Option<ProviderDetection> {
    if title.to_lowercase().contains("action required") {
        Some(ProviderDetection::from_title(
            AgentState::Blocked,
            "title_requests_action",
        ))
    } else if title_has_braille_activity(title) {
        Some(ProviderDetection::from_title(
            AgentState::Working,
            "title_shows_activity",
        ))
    } else {
        None
    }
}

fn is_transcript_view(turn: Lines<'_>) -> bool {
    turn.contains_all(&["to scroll", "pgup/pgdn", "home/end", "q to quit"])
        && turn.contains_any(&["esc to edit prev", "esc/← to edit prev"])
}

fn requests_confirmation(turn: Lines<'_>) -> bool {
    turn.contains_any(&[
        "press enter to confirm",
        "enter to submit answer",
        "enter to submit all",
        "allow command?",
        "allow this command",
    ])
}

fn waits_for_choice(turn: Lines<'_>) -> bool {
    turn.contains_any(&["[y/n]", "yes (y)"])
        || (turn.contains_any(&["do you want to", "would you like to"])
            && turn.contains_any(&["yes", "❯"]))
}

fn has_active_footer(recent: Lines<'_>) -> bool {
    if recent.contains("conversation interrupted") {
        return false;
    }
    recent.any_line(|line| {
        let text = line
            .trim()
            .trim_start_matches(['•', '◦'])
            .trim_start()
            .to_lowercase();
        text.starts_with("working (") && text.contains("esc to interrupt")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EvidenceSource;

    #[test]
    fn current_confirmation_outweighs_old_working_text() {
        let result = detect(
            "project",
            "Working (5s · esc to interrupt)\n› next\nAllow this command?",
        );
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "current_turn_requests_confirmation");
    }

    #[test]
    fn old_confirmation_before_prompt_is_not_current() {
        let result = detect("project", "Allow this command?\n› continue\ncompleted");
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(result.signal, "static_title");
    }

    #[test]
    fn title_activity_is_immediate_working_evidence() {
        let result = detect("⠸ project", "");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.source, EvidenceSource::Title);
    }

    #[test]
    fn transcript_view_requests_state_preservation() {
        let result = detect(
            "",
            "› prompt\nesc to edit prev\n↑/↓ to scroll\npgup/pgdn to move\nhome/end to jump\nq to quit",
        );
        assert!(result.preserve_previous);
        assert_eq!(result.signal, "transcript_view");
    }

    #[test]
    fn generic_navigation_controls_do_not_preserve_state() {
        let result = detect(
            "project",
            "› prompt\n↑/↓ to scroll\npgup/pgdn to move\nhome/end to jump\nq to quit",
        );
        assert!(!result.preserve_previous);
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(result.signal, "static_title");
    }

    #[test]
    fn working_footer_accepts_tmux_rendering_without_bullet() {
        let result = detect("", "Working (1m 15s · esc to interrupt)");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "working_footer");
    }

    #[test]
    fn interrupted_footer_is_not_active_work() {
        let result = detect(
            "",
            "Working (1m 15s · esc to interrupt)\nConversation interrupted",
        );
        assert_eq!(result.state, AgentState::Idle);
        assert!(result.inferred);
    }

    #[test]
    fn long_current_turn_question_is_blocked() {
        let result = detect(
            "",
            "› continue\nDo you want to apply these changes?\ncontext one\ncontext two\ncontext three\n1. Yes\n2. No\n❯",
        );
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "current_turn_waits_for_choice");
    }
}
