use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;

use super::ProviderDetection;
use super::screen::{Lines, VisibleScreen, is_divider, title_has_braille_activity};
use crate::model::AgentState;

pub(super) fn detect(title: &str, content: &str) -> ProviderDetection {
    if title_has_braille_activity(title) || title_has_half_circle_activity(title) {
        return ProviderDetection::from_title(AgentState::Working, "title_shows_activity");
    }

    let prompt = live_prompt(content);
    // Confirmed child-panel descriptions are opaque task text, not foreground
    // activity, permission or alternate-view hints.
    let foreground = prompt
        .as_ref()
        .filter(|prompt| !prompt.children.is_empty())
        .map(|prompt| {
            content
                .lines()
                .take(prompt.foreground_lines)
                .collect::<Vec<_>>()
                .join("\n")
        });
    let screen = VisibleScreen::new(foreground.as_deref().unwrap_or(content));
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

    if prompt.as_ref().is_some_and(has_live_turn_activity) {
        return ProviderDetection::from_screen(
            AgentState::Working,
            "live_turn_activity",
            "before_prompt",
        );
    }

    if prompt
        .as_ref()
        .is_some_and(|prompt| !prompt.children.is_empty())
        || has_ready_prompt(&screen)
    {
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

fn has_live_turn_activity(prompt: &LivePrompt<'_>) -> bool {
    // Only inspect the live block adjoining the prompt. Looking farther back
    // can mistake a previous turn's activity for work that is still running.
    let Some(activity) = prompt.preceding.clone().map(str::trim).find(|line| {
        !line.is_empty()
            && !line.starts_with("⎿ ")
            && *line != "✔ Update installed · Restart to update"
    }) else {
        return false;
    };
    // Activity labels are opaque text. Only the surrounding spinner,
    // ellipsis, elapsed time and token counter define this status line.
    static LIVE_TURN: OnceLock<Regex> = OnceLock::new();
    LIVE_TURN
        .get_or_init(|| {
            Regex::new(
                r"^[·✢✳✶✻✽] \S.*(?:…|\.{3}) \([0-9]+[hms](?: [0-9]+[hms])* · [↓↑] [0-9]+(?:\.[0-9]+)?[kKmM]? tokens\)$",
            )
            .expect("valid Claude live turn pattern")
        })
        .is_match(activity)
}

struct LivePrompt<'a> {
    preceding: std::iter::Rev<std::str::Lines<'a>>,
    children: Vec<ChildRow>,
    foreground_lines: usize,
}

fn live_prompt(content: &str) -> Option<LivePrompt<'_>> {
    let mut lines = content.lines().rev();
    let mut footer = Vec::new();
    let mut has_lower_border = false;
    for line in lines.by_ref() {
        if is_divider(line) {
            has_lower_border = true;
            break;
        }
        footer.push(line.trim());
    }
    // Custom status lines can contain arbitrary text above the built-in footer.
    // Validate the built-in footer and its trailing UI, not the user's format.
    // New output below an old prompt must not resurrect that turn's activity.
    footer.reverse();
    if !has_lower_border {
        return None;
    }
    let (children, marker) = parse_live_footer(&footer)?;
    let foreground_lines = content.lines().count() - footer.len() + marker + 1;
    let mut has_prompt = false;
    let mut has_upper_border = false;
    for line in lines.by_ref() {
        if is_divider(line) {
            has_upper_border = true;
            break;
        }
        has_prompt |= line.trim_start().starts_with('❯');
    }
    if !has_prompt || !has_upper_border {
        return None;
    }
    Some(LivePrompt {
        preceding: lines,
        children,
        foreground_lines,
    })
}

fn parse_live_footer(footer: &[&str]) -> Option<(Vec<ChildRow>, usize)> {
    let marker = footer.iter().rposition(|line| {
        line.starts_with("⏵⏵ ")
            || line.starts_with("⏸ plan mode on")
            || line.starts_with("⏸ manual mode on")
            || matches!(*line, "? shortcuts" | "? for shortcuts")
            || is_navigation_hint(line)
    })?;
    let navigating = is_navigation_hint(footer[marker]);
    let mut main_seen = false;
    let mut children = Vec::new();
    for line in &footer[marker + 1..] {
        // Navigation mode replaces the normal footer and decorates panel rows.
        // The cursor is presentation, not part of a child's identity.
        let line = if navigating {
            line.strip_prefix("❯ ").unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            continue;
        }
        if line
            .strip_prefix("⧉ ")
            .is_some_and(|name| !name.trim().is_empty())
        {
            continue;
        }
        let main = line
            .strip_prefix("● main")
            .or_else(|| line.strip_prefix("◯ main"));
        let is_main = main.is_some_and(|suffix| {
            suffix.is_empty()
                || suffix
                    .trim()
                    .strip_prefix("↑ ")
                    .and_then(|text| text.strip_suffix(" more"))
                    .is_some_and(|count| count.parse::<usize>().is_ok())
        });
        if is_main && !main_seen {
            main_seen = true;
            continue;
        }
        if !main_seen || children.len() >= 64 {
            return None;
        }
        children.push(parse_child_panel_row(line)?);
    }
    ((!main_seen && !navigating) || !children.is_empty()).then_some((children, marker))
}

fn is_navigation_hint(line: &str) -> bool {
    if line == "↑/↓ to select · Enter to view" {
        return true;
    }
    // Default bindings only. Already-viewed rows omit Enter/x; other children
    // show view and stop/clear. Optional hints follow in this fixed order.
    let line = line
        .strip_suffix(" · ctrl+x ctrl+k to stop all agents")
        .unwrap_or(line);
    let line = line.strip_suffix(" · Esc to collapse").unwrap_or(line);
    matches!(
        line,
        "↑/↓ to select" | "Enter to view · x to stop" | "Enter to view · x to clear"
    )
}

#[derive(Debug)]
struct ChildRow {
    key: String,
    counters: Option<(u64, u64)>,
}

fn parse_child_panel_row(line: &str) -> Option<ChildRow> {
    // Selection circles and frozen elapsed/token counters also appear on
    // stopped children. Recognize layout here, never activity.
    static CHILD_ROW: OnceLock<Regex> = OnceLock::new();
    let captures = CHILD_ROW
        .get_or_init(|| {
            Regex::new(
                r"^(?P<tree>[│ ]*[├└] )?[●◯] (?P<label>\S.*?) +(?:(?P<elapsed>[0-9]+[dhms](?: [0-9]+[hms])*)(?: · [↓↑] (?P<tokens>[0-9]+(?:\.[0-9]+)?)(?P<unit>[kKmM]?) tokens)?|idle|waiting|awaiting approval)(?: · [0-9]+ queued)?$",
            )
            .expect("valid Claude child panel pattern")
        })
        .captures(line)?;
    let label = captures.name("label")?.as_str();
    if label.len() > 512 {
        return None;
    }
    let tree = captures.name("tree").map_or("", |tree| tree.as_str());
    let key = format!(
        "{tree}{}",
        label.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let counters = if let Some(elapsed) = captures.name("elapsed") {
        let seconds = crate::detect::parse_duration_seconds(elapsed.as_str())?;
        let tokens = captures
            .name("tokens")
            .map_or(Some(0.0), |value| value.as_str().parse::<f64>().ok())?;
        let multiplier = match captures.name("unit").map_or("", |unit| unit.as_str()) {
            "k" | "K" => 1_000.0,
            "m" | "M" => 1_000_000.0,
            _ => 1.0,
        };
        Some((seconds, (tokens * multiplier) as u64))
    } else {
        None
    };
    Some(ChildRow { key, counters })
}

// Rounded counters can stay unchanged across several normal captures. Keep
// confirmed progress briefly, but never turn a frozen terminal row into work.
const CHILD_PROGRESS_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub(crate) struct ChildProgress {
    children: HashMap<String, ChildObservation>,
}

#[derive(Debug)]
struct ChildObservation {
    counters: (u64, u64),
    advanced_at: Option<Instant>,
}

impl ChildProgress {
    pub(crate) fn apply(
        &mut self,
        detection: &mut crate::detect::Detection,
        screen: Option<&str>,
        fresh: bool,
        now: Instant,
    ) {
        if detection.agent != "Claude"
            || detection.state == AgentState::Blocked
            || detection
                .details
                .as_ref()
                .is_some_and(|details| details.preserve_previous)
            || screen.is_none()
        {
            self.children.clear();
            return;
        }
        if fresh {
            let rows = screen
                .and_then(live_prompt)
                .map_or_else(Vec::new, |prompt| prompt.children);
            let mut next = HashMap::new();
            let mut seen = HashSet::new();
            for row in rows {
                if !seen.insert(row.key.clone()) {
                    // Rendered labels are not IDs. Duplicate labels are ambiguous.
                    self.children.clear();
                    return;
                }
                let Some(counters) = row.counters else {
                    continue;
                };
                let previous = self.children.get(&row.key);
                let advanced_at = previous.and_then(|previous| {
                    if counters.0 < previous.counters.0 || counters.1 < previous.counters.1 {
                        None
                    } else if counters != previous.counters {
                        Some(now)
                    } else {
                        previous.advanced_at
                    }
                });
                next.insert(
                    row.key,
                    ChildObservation {
                        counters,
                        advanced_at,
                    },
                );
            }
            self.children = next;
        }
        let active = self.children.values().any(|child| {
            child.advanced_at.is_some_and(|advanced| {
                now.saturating_duration_since(advanced) < CHILD_PROGRESS_GRACE
            })
        });
        if active && detection.state == AgentState::Idle {
            let observed = ProviderDetection::from_screen(
                AgentState::Working,
                "subagent_progress",
                "agent_panel",
            );
            detection.state = observed.state;
            detection.source = observed.source;
            detection.details = Some(observed.details("Claude"));
        }
    }
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

    fn child_screen(rows: &str) -> String {
        format!("Done.\n────\n❯\n────\n⏵⏵ auto mode on · 2 shells · 1 agent\n● main\n{rows}")
    }

    fn observe_child(
        tracker: &mut ChildProgress,
        screen: Option<&str>,
        fresh: bool,
        now: Instant,
    ) -> AgentState {
        let mut detection =
            crate::detect::detect("claude", "✳ task", screen.unwrap_or_default()).unwrap();
        tracker.apply(&mut detection, screen, fresh, now);
        detection.state
    }

    #[test]
    fn default_navigation_hints_preserve_live_turn_and_child_progress() {
        for hint in [
            "↑/↓ to select",
            "Enter to view · x to stop",
            "↑/↓ to select · Esc to collapse",
            "↑/↓ to select · ctrl+x ctrl+k to stop all agents",
            "↑/↓ to select · Esc to collapse · ctrl+x ctrl+k to stop all agents",
            "Enter to view · x to stop · Esc to collapse",
            "Enter to view · x to stop · ctrl+x ctrl+k to stop all agents",
            "Enter to view · x to stop · Esc to collapse · ctrl+x ctrl+k to stop all agents",
        ] {
            let (main, child) = if hint == "↑/↓ to select" {
                ("❯ ● main", "◯")
            } else if hint.starts_with("↑/↓") {
                ("◯ main", "❯ ●")
            } else {
                ("● main", "❯ ◯")
            };
            let first = format!(
                "Done.\n────\n❯\n────\n{hint}\n{main}\n{child} general-purpose Synthetic task 5s · ↓ 12 tokens"
            );
            let progressed = first.replace("5s", "6s");
            let now = Instant::now();
            let mut tracker = ChildProgress::default();
            for (screen, seconds, expected) in [
                (&first, 0, AgentState::Idle),
                (&progressed, 1, AgentState::Working),
                (&progressed, 3, AgentState::Idle),
            ] {
                assert_eq!(
                    observe_child(
                        &mut tracker,
                        Some(screen),
                        true,
                        now + Duration::from_secs(seconds)
                    ),
                    expected,
                    "hint={hint}, seconds={seconds}"
                );
            }
            let live = first.replace("Done.", "✻ Thinking… (5s · ↓ 12 tokens)");
            let detection = crate::detect::detect("claude", "", &live).unwrap();
            assert_eq!(detection.state, AgentState::Working, "{hint}");
            assert_eq!(
                detection.details.unwrap().signal.as_deref(),
                Some("live_turn_activity")
            );
        }
    }

    #[test]
    fn default_navigation_hints_keep_stopped_children_and_descriptions_idle() {
        for hint in [
            "↑/↓ to select",
            "Enter to view · x to clear",
            "Enter to view · x to clear · Esc to collapse",
            "Enter to view · x to clear · ctrl+x ctrl+k to stop all agents",
            "Enter to view · x to clear · Esc to collapse · ctrl+x ctrl+k to stop all agents",
        ] {
            for label in [
                "Review running command tests",
                "Review waiting for permission handling",
                "Showing detailed transcript ctrl+o to toggle",
                "Select model Enter to set as default Esc to cancel",
                "/btw esc to close",
            ] {
                let (main, child) = if hint == "↑/↓ to select" {
                    ("◯ main", "❯ ●")
                } else {
                    ("● main", "❯ ◯")
                };
                let screen = format!(
                    "Done.\n────\n❯\n────\n{hint}\n{main}\n{child} general-purpose {label} 5s · ↓ 12 tokens"
                );
                let now = Instant::now();
                let mut tracker = ChildProgress::default();
                for seconds in [0, 1, 3] {
                    let mut detection = crate::detect::detect("claude", "", &screen).unwrap();
                    tracker.apply(
                        &mut detection,
                        Some(&screen),
                        true,
                        now + Duration::from_secs(seconds),
                    );
                    assert_eq!(detection.state, AgentState::Idle, "{hint}: {label}");
                    let details = detection.details.unwrap();
                    assert!(details.definitive, "{hint}: {label}");
                    assert!(!details.preserve_previous, "{hint}: {label}");
                }
            }
        }
    }

    #[test]
    fn default_navigation_hints_reject_stale_malformed_or_unsupported_panels() {
        let first =
            "Done.\n────\n❯\n────\n↑/↓ to select\n● main\n◯ general-purpose Synthetic task 5s";
        let progressed = first.replace("5s", "6s");
        for malformed in [
            progressed.replace("\n❯\n", "\n\n"),
            progressed.replace("● main\n", ""),
            progressed.replace("6s", "unknown"),
            format!("{progressed}\nThe task is finished."),
            progressed.replace("↑/↓ to select", "↑/↓ to select · arbitrary hint"),
            progressed.replace("↑/↓ to select", "j/k to select"),
            progressed.replace(
                "↑/↓ to select",
                "↑/↓ to select · ctrl+x ctrl+k to stop all agents · Esc to collapse",
            ),
            progressed.replace("↑/↓ to select", "Enter to view"),
            progressed.replace("● main\n◯ general-purpose Synthetic task 6s", "● main"),
        ] {
            let now = Instant::now();
            let mut tracker = ChildProgress::default();
            observe_child(&mut tracker, Some(first), true, now);
            assert_eq!(
                observe_child(
                    &mut tracker,
                    Some(&progressed),
                    true,
                    now + Duration::from_secs(1)
                ),
                AgentState::Working
            );
            assert_eq!(
                observe_child(
                    &mut tracker,
                    Some(&malformed),
                    true,
                    now + Duration::from_millis(1300)
                ),
                AgentState::Idle,
                "{malformed}"
            );
            let stale = malformed.replace("Done.", "✻ Thinking… (5s · ↓ 12 tokens)");
            assert_eq!(
                crate::detect::detect("claude", "", &stale).unwrap().state,
                AgentState::Idle,
                "{stale}"
            );
        }
    }

    #[test]
    fn child_progress_survives_navigation_mode_and_expires_when_frozen() {
        let now = Instant::now();
        let normal = child_screen("◯ general-purpose Synthetic task 5s · ↓ 12 tokens");
        let navigation = "Done.\n────\n❯\n────\nmodel · project · Context 60% left\n↑/↓ to select · Enter to view\n\n❯ ● main\n❯ ◯ general-purpose Synthetic task 6s · ↓ 12 tokens\n⧉ editor";
        let resumed = normal.replace("5s", "8s");
        for title in ["", "✳ task"] {
            let mut tracker = ChildProgress::default();
            for (screen, seconds, expected) in [
                (normal.as_str(), 0, AgentState::Idle),
                (navigation, 1, AgentState::Working),
                (navigation, 3, AgentState::Idle),
                (resumed.as_str(), 4, AgentState::Working),
            ] {
                let mut detection = crate::detect::detect("claude", title, screen).unwrap();
                tracker.apply(
                    &mut detection,
                    Some(screen),
                    true,
                    now + Duration::from_secs(seconds),
                );
                assert_eq!(
                    detection.state, expected,
                    "title={title:?}, seconds={seconds}"
                );
            }
        }
    }

    #[test]
    fn navigation_panel_preserves_foreground_permission_precedence() {
        let screen = "Done.\n────\n❯\n────\n↑/↓ to select · Enter to view\n❯ ● main\n❯ ◯ general-purpose Review running command and waiting for permission handling 5s";
        let progressed = screen.replace("5s", "6s");
        let now = Instant::now();
        let mut tracker = ChildProgress::default();
        assert_eq!(
            observe_child(&mut tracker, Some(screen), true, now),
            AgentState::Idle
        );
        assert_eq!(
            observe_child(
                &mut tracker,
                Some(&progressed),
                true,
                now + Duration::from_secs(1)
            ),
            AgentState::Working
        );
        let blocked = progressed.replace("↑/↓", "Allow this command?\n↑/↓");
        assert_eq!(
            observe_child(
                &mut tracker,
                Some(&blocked),
                true,
                now + Duration::from_secs(2)
            ),
            AgentState::Blocked
        );
        // A navigation hint without the structured panel cannot validate a turn.
        let hint_only =
            "✻ Thinking… (5s · ↓ 12 tokens)\n────\n❯\n────\n↑/↓ to select · Enter to view";
        assert_eq!(
            crate::detect::detect("claude", "✳ task", hint_only)
                .unwrap()
                .state,
            AgentState::Idle
        );
    }

    #[test]
    fn child_progress_requires_advancement_and_expires_when_frozen() {
        let now = Instant::now();
        let mut tracker = ChildProgress::default();
        let first = child_screen("◯ general-purpose Synthetic task 5s · ↓ 12 tokens");
        let next = child_screen("◯ general-purpose Synthetic task 6s · ↓ 12 tokens");
        for (screen, fresh, millis, expected) in [
            (&first, true, 0, AgentState::Idle),
            (&first, true, 300, AgentState::Idle),
            (&next, false, 600, AgentState::Idle),
            (&next, true, 1000, AgentState::Working),
            (&next, true, 2000, AgentState::Working),
            (&next, false, 2999, AgentState::Working),
            (&next, true, 3000, AgentState::Idle),
        ] {
            assert_eq!(
                observe_child(
                    &mut tracker,
                    Some(screen),
                    fresh,
                    now + Duration::from_millis(millis)
                ),
                expected,
                "at {millis}"
            );
        }
    }

    #[test]
    fn child_progress_uses_tokens_and_ignores_selection_or_reordering() {
        let now = Instant::now();
        let mut tracker = ChildProgress::default();
        let first = child_screen(
            "◯ general-purpose First task 5s · ↓ 999 tokens\n◯ general-purpose Second task 8s",
        );
        let reordered = child_screen(
            "● general-purpose Second task 8s\n◯ general-purpose First task 5s · ↓ 999 tokens",
        );
        let progressed = reordered.replace("999 tokens", "1.0k tokens");
        assert_eq!(
            observe_child(&mut tracker, Some(&first), true, now),
            AgentState::Idle
        );
        assert_eq!(
            observe_child(
                &mut tracker,
                Some(&reordered),
                true,
                now + Duration::from_millis(300)
            ),
            AgentState::Idle
        );
        assert_eq!(
            observe_child(
                &mut tracker,
                Some(&progressed),
                true,
                now + Duration::from_millis(600)
            ),
            AgentState::Working
        );
    }

    #[test]
    fn child_descriptions_do_not_supply_foreground_ui_signals() {
        for label in [
            "Review running command tests",
            "Review waiting for permission handling",
            "Showing detailed transcript ctrl+o to toggle",
            "Select model Enter to set as default Esc to cancel",
            "/btw esc to close",
        ] {
            let screen = child_screen(&format!("◯ general-purpose {label} waiting"));
            let detection = crate::detect::detect("claude", "", &screen).unwrap();
            assert_eq!(detection.state, AgentState::Idle, "{label}");
            let details = detection.details.unwrap();
            assert!(details.definitive, "{label}");
            assert!(!details.preserve_previous, "{label}");
        }
        let screen = child_screen("◯ general-purpose Synthetic task waiting")
            .replace("⏵⏵", "Allow this command?\n⏵⏵");
        assert_eq!(
            crate::detect::detect("claude", "", &screen).unwrap().state,
            AgentState::Blocked
        );
    }

    #[test]
    fn child_progress_clears_for_waiting_missing_ambiguous_or_blocked_frames() {
        let now = Instant::now();
        let first = child_screen("◯ general-purpose Synthetic task 5s · ↓ 12 tokens");
        let next = first.replace("5s", "6s");
        let blocked = next.replace("⏵⏵", "Allow this command?\n⏵⏵");
        let duplicates = child_screen(
            "◯ general-purpose Synthetic task 7s\n● general-purpose Synthetic task 8s",
        );
        for (replacement, expected) in [
            (
                Some(child_screen("◯ general-purpose Synthetic task waiting")),
                AgentState::Idle,
            ),
            (
                Some(child_screen("◯ general-purpose Synthetic task idle")),
                AgentState::Idle,
            ),
            (
                Some(child_screen(
                    "◯ general-purpose Synthetic task awaiting approval",
                )),
                AgentState::Idle,
            ),
            (
                Some("Done.\n────\n❯\n────\n? shortcuts · 2 shells · 1 agent".into()),
                AgentState::Idle,
            ),
            (Some(duplicates), AgentState::Idle),
            (Some(blocked), AgentState::Blocked),
            (Some(next.replace("6s", "1s")), AgentState::Idle),
            (None, AgentState::Idle),
        ] {
            let mut tracker = ChildProgress::default();
            observe_child(&mut tracker, Some(&first), true, now);
            assert_eq!(
                observe_child(
                    &mut tracker,
                    Some(&next),
                    true,
                    now + Duration::from_secs(1)
                ),
                AgentState::Working
            );
            assert_eq!(
                observe_child(
                    &mut tracker,
                    replacement.as_deref(),
                    true,
                    now + Duration::from_millis(1300)
                ),
                expected
            );
        }
    }

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
    fn live_turn_requires_a_footer_marker() {
        for footer in ["", "\n   \n", "custom status without a footer marker"] {
            let screen = format!("· Simmering… (55s · ↓ 3.8k tokens)\n────\n❯\n────\n{footer}");
            assert_eq!(detect("✳ task", &screen).state, AgentState::Idle);
        }
    }

    #[test]
    fn live_turn_with_auxiliary_status_lines_is_working() {
        let screen = "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)\n  ⎿  Comparing alternatives…\n  ⎿ Tip: Use /btw to ask a side question\n────\n❯\n────\n? shortcuts";
        let result = detect("✳ task", screen);
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.signal, "live_turn_activity");

        let completed = screen.replace(
            "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)",
            "✻ Worked for 55s · done · 2 shells still running",
        );
        assert_eq!(detect("✳ task", &completed).state, AgentState::Idle);
    }

    #[test]
    fn live_turn_with_custom_status_line_is_working() {
        for status in ["project · branch", "custom model\ncustom cost and usage"] {
            let screen = format!(
                "· Simmering… (55s · ↓ 3.8k tokens)\n────\n❯\n────\n{status}\n⏵⏵ auto mode on · 2 shells · ← 1 agent"
            );
            let result = detect("✳ task", &screen);
            assert_eq!(result.state, AgentState::Working, "{status}");
            assert_eq!(result.signal, "live_turn_activity");
            let completed = screen.replace(
                "· Simmering… (55s · ↓ 3.8k tokens)",
                "✻ Worked for 55s · done · 2 shells still running",
            );
            assert_eq!(detect("✳ task", &completed).state, AgentState::Idle);
        }
    }

    #[test]
    fn live_turn_accepts_builtin_modes_but_not_mode_hints_in_child_descriptions() {
        for footer in [
            "⏸ plan mode on (shift+tab to cycle)",
            "⏸ manual mode on",
            "⏵⏵ accept edits on (shift+tab to cycle)",
            "⏵⏵ don't ask on",
        ] {
            let screen =
                format!("✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)\n────\n❯\n────\n{footer}");
            assert_eq!(detect("✳ task", &screen).state, AgentState::Working);
        }
        let screen = "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)\n────\n❯\n────\n● main\n◯ general-purpose Review shift+tab hints 1s";
        assert_eq!(detect("✳ task", screen).state, AgentState::Idle);
    }

    #[test]
    fn trailing_ui_preserves_foreground_activity_without_implying_child_activity() {
        for trailing in [
            "⧉ synthetic-workspace",
            "● main\n◯ general-purpose Synthetic task 1m 40s · ↓ 132.2k tokens",
            "⧉ synthetic-workspace\n◯ main\n● general-purpose Synthetic task 1m 40s · ↓ 132.2k tokens",
            "● main\n◯ general-purpose Synthetic task waiting",
            "● main\n◯ general-purpose Synthetic task idle",
            "● main   ↑ 2 more\n◯ general-purpose Synthetic task 1s\n└ ◯ general-purpose Nested task 0s",
        ] {
            let screen = format!(
                "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)\n────\n❯\n────\n⏵⏵ auto mode on · 1 agent\n{trailing}"
            );
            let result = detect("✳ task", &screen);
            assert_eq!(result.state, AgentState::Working, "{trailing}");
            assert_eq!(result.signal, "live_turn_activity");
            let completed = screen.replace(
                "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)",
                "✻ Worked for 11m 24s · done · 2 shells still running",
            );
            assert_eq!(
                detect("✳ task", &completed).state,
                AgentState::Idle,
                "{trailing}"
            );
        }
    }

    #[test]
    fn trailing_output_or_unanchored_panel_does_not_resurrect_foreground_activity() {
        for trailing in [
            "The task is finished.",
            "◯ general-purpose Synthetic task 1m 40s · ↓ 132.2k tokens",
            "● main\nUnknown panel output",
            "● main\n◯ general-purpose Synthetic task 1m 40s · ↓ 132.2k tokens\nThe task is finished.",
            "⧉ synthetic-workspace\nThe task is finished.",
        ] {
            let screen = format!(
                "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)\n────\n❯\n────\n⏵⏵ auto mode on · 1 agent\n{trailing}"
            );
            assert_eq!(
                detect("✳ task", &screen).state,
                AgentState::Idle,
                "{trailing}"
            );
        }
        let unanchored = "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)\n────\n❯\n────\n● main\n◯ general-purpose Synthetic task 1m 40s · ↓ 132.2k tokens";
        assert_eq!(detect("✳ task", unanchored).state, AgentState::Idle);
    }

    #[test]
    fn live_turn_does_not_depend_on_the_activity_verb_or_title() {
        for activity in [
            "· Simmering… (55s · ↓ 3.8k tokens)",
            "✻ Pondering… (1m 14s · ↑ 120 tokens)",
            "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)",
            "✶ C’est parti!… (8s · ↓ 12 tokens)",
            "✽ 調査中… (8s · ↓ 12 tokens)",
            "✳ 🚀 custom / phase (2)... (8s · ↓ 12 tokens)",
        ] {
            let screen = format!("{activity}\n────\n❯ draft\n────\n? shortcuts");
            let result = detect("", &screen);
            assert_eq!(result.state, AgentState::Working, "{activity}");
            assert_eq!(result.signal, "live_turn_activity");
        }
    }

    #[test]
    fn live_turn_still_requires_spinner_elapsed_time_and_tokens() {
        for activity in [
            "Beboppin'… (11m 24s · ↓ 53.9k tokens)",
            "✢ Beboppin'… (↓ 53.9k tokens)",
            "✢ Beboppin'… (11m 24s)",
            "✢ Beboppin'… (11m 24s · ↓ many tokens)",
            "✢ … (11m 24s · ↓ 53.9k tokens)",
        ] {
            let screen = format!("{activity}\n────\n❯\n────\n? shortcuts");
            assert_eq!(
                detect("✳ task", &screen).state,
                AgentState::Idle,
                "{activity}"
            );
        }
    }

    #[test]
    fn historical_or_quoted_live_turn_lines_do_not_mark_working() {
        let activity = "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)";
        for screen in [
            format!("{activity}\n{MODERN_READY_SCREEN}"),
            format!("> {activity}\n────\n❯\n────\n? shortcuts"),
            format!("────\n❯ explain this\n{activity}\n────\n? shortcuts"),
            format!("{activity}\n────\n❯ previous\n────\nThe task is finished."),
        ] {
            assert_eq!(
                detect("✳ task", &screen).state,
                AgentState::Idle,
                "{screen}"
            );
        }
    }

    #[test]
    fn permissions_and_alternate_views_override_live_turn_lines() {
        let screen = "✢ Beboppin'… (11m 24s · ↓ 53.9k tokens)\n────\n❯\n────\n";
        let permission = detect(
            "✳ task",
            &format!("{screen}Allow this command?\n? shortcuts"),
        );
        assert_eq!(permission.state, AgentState::Blocked);
        let transcript = detect(
            "✳ task",
            &format!("{screen}Showing detailed transcript\nctrl+o to toggle"),
        );
        assert!(transcript.preserve_previous);
    }

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
