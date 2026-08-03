use super::ProviderDetection;
use super::screen::{Lines, VisibleScreen, opencode_prompt};
use crate::model::AgentState;

pub(super) fn detect(_title: &str, content: &str) -> ProviderDetection {
    let screen = VisibleScreen::new(content);
    let current = screen.at_last(opencode_prompt);
    if requests_permission(current) {
        return ProviderDetection::from_screen(
            AgentState::Blocked,
            "permission_panel",
            "current_interaction",
        );
    }

    if shows_interrupt_hint(current) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "interrupt_hint",
            "current_interaction",
        );
    }

    if current.any_line(has_progress_pattern) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "progress_pattern",
            "current_interaction",
        );
    }

    if screen.all().any_line(opencode_prompt) {
        return ProviderDetection::from_screen(
            AgentState::Idle,
            "input_prompt",
            "current_interaction",
        );
    }

    ProviderDetection::inferred_idle("opencode_foreground_without_activity")
}

fn requests_permission(current: Lines<'_>) -> bool {
    current.contains("permission required")
        || (current.contains("esc dismiss")
            && current.contains_any(&["enter confirm", "enter submit", "enter toggle"])
            && current.contains_any(&["select", "tab"]))
}

fn shows_interrupt_hint(current: Lines<'_>) -> bool {
    current.contains_any(&[
        "esc interrupt",
        "esc to interrupt",
        "esc again to interrupt",
        "ctrl+c to interrupt",
        "press esc to interrupt",
    ])
}

fn has_progress_pattern(line: &str) -> bool {
    let longest = line
        .chars()
        .fold((0usize, 0usize), |(longest, current), character| {
            if matches!(character, '■' | '⬝') {
                (longest.max(current + 1), current + 1)
            } else {
                (longest, 0)
            }
        })
        .0;
    longest >= 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_after_prompt_is_blocked() {
        let result = detect("", "Ask anything...\nPermission required\nEsc dismiss");
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "permission_panel");
    }

    #[test]
    fn stale_permission_before_latest_prompt_is_ignored() {
        let result = detect("", "Permission required\nrejected\nAsk anything...");
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(result.signal, "input_prompt");
    }

    #[test]
    fn interrupt_hint_after_prompt_is_working() {
        let result = detect("", "Ask anything...\nBuilding\nesc interrupt");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "interrupt_hint");
    }

    #[test]
    fn progress_pattern_is_working() {
        let result = detect("", "Ask anything...\n■⬝■⬝");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "progress_pattern");
    }

    #[test]
    fn repeated_escape_interrupt_hint_is_working() {
        let result = detect("", "Ask anything...\nBuilding\nesc again to interrupt");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "interrupt_hint");
    }
}
