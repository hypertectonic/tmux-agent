use super::ProviderDetection;
use super::screen::{Lines, VisibleScreen};
use crate::model::AgentState;

const WORKING_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub(super) fn detect(title: &str, content: &str) -> ProviderDetection {
    if let Some((state, signal, _)) = state_title(title) {
        return ProviderDetection::from_title(state, signal);
    }

    let screen = VisibleScreen::new(content);
    let recent = screen.recent_non_empty(16);
    if is_permission_prompt(recent) {
        return ProviderDetection::from_screen(
            AgentState::Blocked,
            "permission_selector",
            "current_selector",
        );
    }
    if recent.any_line(is_active_status) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "status_indicator",
            "recent_lines",
        );
    }

    ProviderDetection::inferred_idle("omp_foreground_without_activity")
}

pub(super) fn stable_title(title: &str) -> Option<String> {
    state_title(title).map(|(_, _, label)| label.to_string())
}

fn state_title(title: &str) -> Option<(AgentState, &'static str, &str)> {
    let remainder = title.strip_prefix("π ")?;
    let mut characters = remainder.char_indices();
    let (_, separator) = characters.next()?;
    let label = match characters.next() {
        None => "",
        Some((index, ' ')) if index + 1 < remainder.len() => &remainder[index + 1..],
        Some(_) => return None,
    };
    if label.starts_with(' ') {
        return None;
    }

    match separator {
        '!' => Some((AgentState::Blocked, "state_title_attention", label)),
        '>' => Some((AgentState::Idle, "state_title_idle", label)),
        frame if WORKING_FRAMES.contains(&frame) => {
            Some((AgentState::Working, "state_title_working", label))
        }
        _ => None,
    }
}

fn is_permission_prompt(recent: Lines<'_>) -> bool {
    recent.contains("allow?")
        && recent.any_line(|line| is_selector_option(line, "yes"))
        && recent.any_line(|line| is_selector_option(line, "no"))
        && recent.contains_all(&["navigate", "select", "cancel"])
}

fn is_selector_option(line: &str, option: &str) -> bool {
    line.trim_start()
        .strip_prefix('→')
        .unwrap_or(line.trim_start())
        .trim()
        .eq_ignore_ascii_case(option)
}

fn is_active_status(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(spinner) = trimmed.chars().next() else {
        return false;
    };
    if !WORKING_FRAMES.contains(&spinner) {
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
    fn only_exact_state_titles_are_recognized() {
        assert_eq!(
            stable_title("π ⠋ local-bench").as_deref(),
            Some("local-bench")
        );
        assert_eq!(
            stable_title("π ⠏ local-bench").as_deref(),
            Some("local-bench")
        );
        assert!(stable_title("π custom title").is_none());
        assert!(stable_title("π ⣿ local-bench").is_none());
        assert!(stable_title("π > ").is_none());
        assert!(stable_title("prefix π > local-bench").is_none());
    }
}
