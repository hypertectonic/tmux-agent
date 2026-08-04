use super::ProviderDetection;
use super::screen::{Lines, VisibleScreen};
use crate::model::AgentState;

pub(super) fn detect(_title: &str, content: &str) -> ProviderDetection {
    let screen = VisibleScreen::new(content);
    let recent = screen.recent_non_empty(16);

    if is_project_trust_prompt(recent) {
        return ProviderDetection::from_screen(
            AgentState::Blocked,
            "project_trust_prompt",
            "current_selector",
        );
    }

    if is_permission_prompt(recent) {
        return ProviderDetection::from_screen(
            AgentState::Blocked,
            "permission_selector",
            "current_selector",
        );
    }

    if is_selector(recent) {
        return ProviderDetection::preserve("interactive_selector", "current_selector");
    }

    if recent.any_line(is_active_status) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "status_indicator",
            "recent_lines",
        );
    }

    ProviderDetection::inferred_idle("pi_foreground_without_activity")
}

fn is_project_trust_prompt(recent: Lines<'_>) -> bool {
    recent.contains_all(&["project trust", "saved decision:", "current session:"])
        && recent.contains_all(&["navigate", "save", "cancel"])
}

fn is_permission_prompt(recent: Lines<'_>) -> bool {
    recent.contains("allow?")
        && recent.any_line(|line| is_selector_option(line, "yes"))
        && recent.any_line(|line| is_selector_option(line, "no"))
        && is_selector(recent)
}

fn is_selector_option(line: &str, option: &str) -> bool {
    line.trim_start()
        .strip_prefix('→')
        .unwrap_or(line.trim_start())
        .trim()
        .eq_ignore_ascii_case(option)
}

fn is_selector(recent: Lines<'_>) -> bool {
    recent.contains_all(&["navigate", "select", "cancel"])
}

fn is_active_status(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(spinner) = trimmed.chars().next() else {
        return false;
    };
    if !('\u{2801}'..='\u{28ff}').contains(&spinner) {
        return false;
    }

    let status = trimmed[spinner.len_utf8()..]
        .trim_start()
        .to_ascii_lowercase();
    [
        "working...",
        "retrying (",
        "compacting context...",
        "auto-compacting...",
        "context overflow detected, auto-compacting...",
        "summarizing branch...",
        "running...",
    ]
    .iter()
    .any(|marker| status.starts_with(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_pi_title_does_not_imply_activity() {
        let result = detect("π - project", "");
        assert_eq!(result.state, AgentState::Idle);
        assert!(result.inferred);
    }

    #[test]
    fn ordinary_text_that_mentions_working_is_not_activity() {
        let result = detect("π - project", "Assistant response: Working... on it");
        assert_eq!(result.state, AgentState::Idle);
        assert!(result.inferred);
    }

    #[test]
    fn default_loader_is_working() {
        let result = detect("π - project", "⠋ Working... (esc to interrupt)");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "status_indicator");
    }

    #[test]
    fn permission_selector_is_blocked() {
        let result = detect(
            "π - project",
            "Allow?\n→ Yes\n  No\n↑↓ navigate  Enter select  Esc cancel",
        );
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(result.signal, "permission_selector");
    }
}
