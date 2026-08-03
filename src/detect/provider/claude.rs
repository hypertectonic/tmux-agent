use super::ProviderDetection;
use super::screen::{Lines, VisibleScreen, title_has_braille_activity};
use crate::model::AgentState;

pub(super) fn detect(title: &str, content: &str) -> ProviderDetection {
    if title_has_braille_activity(title) {
        return ProviderDetection::from_title(AgentState::Working, "title_shows_activity");
    }

    let screen = VisibleScreen::new(content);
    let recent = screen.recent_non_empty(6);
    if let Some(signal) = alternate_view(&screen, recent) {
        let scope = if signal == "model_picker" {
            "visible_screen"
        } else {
            "recent_lines"
        };
        return ProviderDetection::preserve(signal, scope);
    }

    if is_background_task_overlay(recent) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "background_task_overlay",
            "recent_lines",
        );
    }

    let current_panel = screen.after_last_divider();
    if let Some(signal) = blocking_signal(current_panel) {
        return ProviderDetection::from_screen(AgentState::Blocked, signal, "current_panel");
    }

    if recent.contains_any(&[
        "esc to interrupt",
        "ctrl+c to stop",
        "working (",
        "thinking (",
        "running command",
    ]) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "recent_activity_marker",
            "recent_lines",
        );
    }

    if has_ready_prompt(&screen) {
        return ProviderDetection::from_screen(AgentState::Idle, "input_prompt", "prompt_box");
    }

    if title.trim_start().starts_with('✳') {
        return ProviderDetection::from_title(AgentState::Idle, "ready_title");
    }

    ProviderDetection::inferred_idle("claude_foreground_without_activity")
}

fn alternate_view(screen: &VisibleScreen<'_>, recent: Lines<'_>) -> Option<&'static str> {
    if recent.contains("showing detailed transcript")
        && recent.contains_any(&["to toggle", "show all", "collapse", "scroll", "shortcuts"])
    {
        return Some("transcript_view");
    }
    let visible = screen.all();
    if visible.contains_all(&["select model", "enter to set as default", "esc to cancel"])
        && !visible.contains("do you want to proceed?")
    {
        return Some("model_picker");
    }
    None
}

fn is_background_task_overlay(recent: Lines<'_>) -> bool {
    recent.any_line(|line| line.trim_start().starts_with("/btw")) && recent.contains("esc to close")
}

fn blocking_signal(panel: Lines<'_>) -> Option<&'static str> {
    if panel.contains_all(&["enter to select", "esc to cancel"])
        && panel.contains_any(&[
            "arrow keys to navigate",
            "arrows to navigate",
            "to navigate",
        ])
    {
        return Some("interactive_form");
    }
    if panel.contains_all(&["run a dynamic workflow?", "esc to cancel"]) {
        return Some("workflow_confirmation");
    }
    panel
        .contains_any(&[
            "do you want to proceed?",
            "allow this command?",
            "waiting for permission",
            "do you want to allow this connection?",
            "would you like to continue?",
            "review your answers",
            "skip interview and plan immediately",
            "tab to amend",
            "ctrl+e to explain",
        ])
        .then_some("permission_question")
}

fn has_ready_prompt(screen: &VisibleScreen<'_>) -> bool {
    let footer_is_passive = !screen.after_last_divider().any_line(|line| {
        let text = line.trim().to_lowercase();
        !text.is_empty()
            && !text.contains("shortcuts")
            && !text.contains("bypass permissions")
            && !text.contains("shift+tab")
    });
    footer_is_passive
        && screen
            .latest_prompt_box()
            .is_some_and(|body| body.any_line(|line| line.trim_start().starts_with('❯')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EvidenceSource;

    #[test]
    fn permission_question_is_blocked() {
        let result = detect("", "Do you want to proceed?\n1. Yes\n2. No\nEsc to cancel");
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "permission_question");
    }

    #[test]
    fn input_prompt_is_direct_idle_evidence() {
        let result = detect("", "response\n────────\n❯ \n────────\n? shortcuts");
        assert_eq!(result.state, AgentState::Idle);
        assert!(result.definitive);
        assert_eq!(result.signal, "input_prompt");
    }

    #[test]
    fn recent_activity_outweighs_an_old_prompt() {
        let result = detect(
            "",
            "────────\n❯ previous\n────────\nRunning command\nesc to interrupt",
        );
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "recent_activity_marker");
    }

    #[test]
    fn live_permission_outweighs_older_activity() {
        let result = detect(
            "",
            "Running command\n────────\nDo you want to proceed?\n1. Yes\n2. No",
        );
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "permission_question");
    }

    #[test]
    fn permission_below_a_stale_prompt_box_is_blocked() {
        let result = detect(
            "",
            "────────\n❯ previous prompt\n────────\nAllow this command?",
        );
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "permission_question");
    }

    #[test]
    fn supported_permission_wording_is_blocked() {
        for content in [
            "Allow this command?",
            "Waiting for permission",
            "Do you want to allow this connection?",
            "Would you like to continue?\n1. Yes\n2. No",
            "Review your answers\nEnter to select · Esc to cancel",
            "Skip interview and plan immediately\nEnter to select · Esc to cancel",
            "Command contains expansion\nTab to amend · Ctrl+E to explain",
        ] {
            let result = detect("", content);
            assert_eq!(result.state, AgentState::Blocked, "{content}");
            assert_eq!(result.signal, "permission_question", "{content}");
        }
    }

    #[test]
    fn transcript_and_model_views_preserve_state() {
        let transcript = detect("", "Showing detailed transcript\nctrl+o to toggle");
        assert!(transcript.preserve_previous);

        let picker = detect("", "Select model\nEnter to set as default\nEsc to cancel");
        assert!(picker.preserve_previous);
    }

    #[test]
    fn braille_title_marks_active_work() {
        let result = detect("⠂ project", "");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.source, EvidenceSource::Title);
    }
}
