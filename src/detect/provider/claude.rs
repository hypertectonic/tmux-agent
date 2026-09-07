use super::ProviderDetection;
use super::screen::{Lines, VisibleScreen, title_has_braille_activity};
use crate::model::AgentState;

pub(super) fn detect(title: &str, content: &str) -> ProviderDetection {
    if title_has_braille_activity(title) || title_has_half_circle_activity(title) {
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

fn title_has_half_circle_activity(title: &str) -> bool {
    matches!(title.trim_start().chars().next(), Some('◐' | '◑'))
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
    let footer = screen.after_last_divider();
    let footer_is_passive = !footer.any_line(|line| {
        let text = line.trim().to_lowercase();
        !text.is_empty()
            && !text.contains("shortcuts")
            && !text.contains("bypass permissions")
            && !text.contains("shift+tab")
    });
    (footer_is_passive || is_modern_ready_footer(footer))
        && screen
            .latest_prompt_box()
            .is_some_and(|body| body.any_line(|line| line.trim_start().starts_with('❯')))
}

fn is_modern_ready_footer(footer: Lines<'_>) -> bool {
    // The observed footer has exactly a model/project/branch/context row and
    // an auto-mode row. Background shell counts describe jobs, not the turn.
    let mut rows = footer.iter().map(str::trim).filter(|line| !line.is_empty());
    let (Some(status), Some(mode), None) = (rows.next(), rows.next(), rows.next()) else {
        return false;
    };
    let status: Vec<_> = status.split('·').map(str::trim).collect();
    let [model, project, branch, context] = status.as_slice() else {
        return false;
    };
    if [model, project, branch]
        .iter()
        .any(|field| field.is_empty())
        || !context
            .strip_prefix("Context ")
            .and_then(|text| text.strip_suffix("% left"))
            .and_then(|percent| percent.parse::<u8>().ok())
            .is_some_and(|percent| percent <= 100)
    {
        return false;
    }

    let mut fields = mode.split('·').map(str::trim);
    fields.next() == Some("⏵⏵ auto mode on")
        && fields.all(|field| {
            let field = field.strip_prefix("← ").unwrap_or(field);
            let Some((count, label)) = field.split_once(' ') else {
                return false;
            };
            count.parse::<u64>().is_ok() && matches!(label, "shell" | "shells" | "agent" | "agents")
        })
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

    const MODERN_READY_SCREEN: &str = "Done.\n✻ Worked for 46m · done · 1 shell still running\n────\n❯ editable unsent text\n────\nmodel · project · main · Context 23% left\n⏵⏵ auto mode on · 1 shell · ← 1 agent";

    #[test]
    fn modern_prompt_without_title_is_direct_idle_evidence() {
        for screen in [
            MODERN_READY_SCREEN.to_string(),
            MODERN_READY_SCREEN.replace(" · 1 shell", ""),
        ] {
            let result = detect("", &screen);
            assert_eq!(result.state, AgentState::Idle);
            assert_eq!(result.source, EvidenceSource::Screen);
            assert_eq!(result.signal, "input_prompt");
            assert!(result.definitive);
            assert!(!result.inferred);
        }
    }

    #[test]
    fn foreground_activity_and_permissions_still_override_ready_shell_footer() {
        for title in ["◐ task", "◑ task", "⠂ task"] {
            let result = detect(title, MODERN_READY_SCREEN);
            assert_eq!(result.state, AgentState::Working, "{title}");
            assert_eq!(result.signal, "title_shows_activity");
        }
        let active = detect(
            "✳ task",
            &format!("{MODERN_READY_SCREEN}\nesc to interrupt"),
        );
        assert_eq!(active.state, AgentState::Working);
        assert_eq!(active.signal, "recent_activity_marker");

        let permission = detect(
            "✳ task",
            &format!("{MODERN_READY_SCREEN}\nAllow this command?"),
        );
        assert_eq!(permission.state, AgentState::Blocked);
        assert_eq!(permission.signal, "permission_question");
    }

    #[test]
    fn historical_prompts_and_output_do_not_supply_modern_ready_evidence() {
        for screen in [
            "────\n❯ previous\n────\nordinary output",
            "────\n❯ previous\n────\nordinary output\nmodel · project · main · Context 23% left\n⏵⏵ auto mode on · 1 shell",
            "────\n❯ previous\n────\nmodel · project · main · Context 23% left\n⏵⏵ auto mode on · 1 shell\nnew output",
            "────\n❯ previous\n────\noutput mentioning Context 23% left\n⏵⏵ auto mode on · 1 shell",
            "────\n❯ previous\n────\nmodel · project · main · Context 23% left\n⏵⏵ auto mode on · arbitrary output",
            "────\n❯ previous\n────\nmodel · project · main · Context 23% left",
            "────\n❯ previous\n────\n⏵⏵ auto mode on · 1 shell",
            "❯ previous\nmodel · project · main · Context 23% left\n⏵⏵ auto mode on · 1 shell",
        ] {
            let result = detect("", screen);
            assert!(!result.definitive, "{screen}");
            assert!(result.inferred, "{screen}");
            assert_ne!(result.signal, "input_prompt", "{screen}");
        }
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
