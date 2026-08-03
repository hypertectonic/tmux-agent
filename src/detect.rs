mod provider;
pub(crate) mod stabilize;

use crate::model::{AgentState, DetectionDetails, EvidenceSource, GoalInfo, GoalState};
use regex::Regex;
use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub agent: String,
    pub state: AgentState,
    pub source: EvidenceSource,
    pub goal: Option<GoalInfo>,
    pub details: Option<DetectionDetails>,
}

pub fn looks_like_agent(processes: &str) -> bool {
    identify_agent(processes).is_some()
}

pub fn agent_for_argv(command: &[OsString]) -> Option<String> {
    let executable = command.first()?;
    if let Some(agent) = agent_for_os_program(executable) {
        return Some(agent.to_string());
    }

    let runtime = program_name_os(executable);
    if !matches!(
        runtime.as_str(),
        "node" | "nodejs" | "bun" | "deno" | "python" | "python3" | "npx" | "bunx" | "env"
    ) {
        return None;
    }

    if runtime == "env" {
        return env_wrapped_command(command).and_then(agent_for_argv);
    }

    let candidates = command[1..]
        .iter()
        .filter(|field| {
            let field = field.to_string_lossy();
            !field.starts_with('-') && !field.contains('=')
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates
        .first()
        .and_then(|field| agent_for_os_program(field))
        .map(str::to_string)
}

fn env_wrapped_command(command: &[OsString]) -> Option<&[OsString]> {
    let mut index = 1;
    while index < command.len() {
        let argument = command[index].to_string_lossy();
        if argument == "--" {
            return command.get(index + 1..).filter(|values| !values.is_empty());
        }
        if matches!(
            argument.as_ref(),
            "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
        ) {
            index += 2;
            continue;
        }
        if argument.starts_with("--unset=")
            || argument.starts_with("--chdir=")
            || argument.starts_with("--split-string=")
            || argument.starts_with('-')
            || argument.contains('=')
        {
            index += 1;
            continue;
        }
        return Some(&command[index..]);
    }
    None
}

pub fn detect(processes: &str, title: &str, screen: &str) -> Option<Detection> {
    let agent = identify_agent(processes)?;
    Some(detect_agent(agent, title, screen))
}

pub fn detect_agent(agent: String, title: &str, screen: &str) -> Detection {
    let goal = (agent == "Codex")
        .then(|| detect_codex_goal(screen))
        .flatten();
    let detected = provider::detect(&agent, title, screen)
        .expect("identified agents must have a typed provider detector");
    let details = detected.details(&agent);
    Detection {
        agent,
        state: detected.state,
        source: detected.source,
        goal,
        details: Some(details),
    }
}

fn detect_codex_goal(screen: &str) -> Option<GoalInfo> {
    static GOAL: OnceLock<Regex> = OnceLock::new();
    let pattern = GOAL.get_or_init(|| {
        Regex::new(r"(Pursuing goal|Goal achieved) \(([0-9]+[dhms](?:\s+[0-9]+[dhms])*)\)\s*$")
            .expect("Codex goal footer regex is valid")
    });
    screen
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| {
            let captures = pattern.captures(line)?;
            let state = match captures.get(1)?.as_str() {
                "Pursuing goal" => GoalState::Pursuing,
                "Goal achieved" => GoalState::Achieved,
                _ => return None,
            };
            let elapsed_seconds = parse_goal_duration(captures.get(2)?.as_str())?;
            Some(GoalInfo {
                state,
                elapsed_seconds,
                achievement_pending: false,
                achievement_observed_at_ms: 0,
            })
        })
}

fn parse_goal_duration(value: &str) -> Option<u64> {
    let mut total = 0_u64;
    let mut parsed = false;
    for component in value.split_whitespace() {
        let (number, multiplier) = match component.as_bytes().last().copied()? {
            b'd' => (&component[..component.len() - 1], 86_400_u64),
            b'h' => (&component[..component.len() - 1], 3_600_u64),
            b'm' => (&component[..component.len() - 1], 60_u64),
            b's' => (&component[..component.len() - 1], 1_u64),
            _ => return None,
        };
        let number = number.parse::<u64>().ok()?;
        total = total.checked_add(number.checked_mul(multiplier)?)?;
        parsed = true;
    }
    parsed.then_some(total)
}

fn identify_agent(processes: &str) -> Option<String> {
    processes.lines().find_map(agent_for_command)
}

fn agent_for_command(command: &str) -> Option<String> {
    let mut fields = command.split_whitespace();
    let executable = fields.next()?;
    if let Some(agent) = agent_for_program(executable) {
        return Some(agent.to_string());
    }

    let runtime = program_name(executable);
    if !matches!(
        runtime.as_str(),
        "node" | "nodejs" | "bun" | "deno" | "python" | "python3" | "npx" | "bunx" | "env"
    ) {
        return None;
    }

    let candidates = fields
        .filter(|field| !field.starts_with('-') && !field.contains('='))
        .collect::<Vec<_>>();
    if runtime == "env" {
        return agent_for_command(&candidates.join(" "));
    }
    candidates
        .first()
        .and_then(|field| agent_for_program(field))
        .map(str::to_string)
}

fn agent_for_program(program: &str) -> Option<&'static str> {
    let name = program_name(program);
    agent_for_name(&name)
}

fn agent_for_os_program(program: &OsStr) -> Option<&'static str> {
    let name = program_name_os(program);
    agent_for_name(&name)
}

fn agent_for_name(name: &str) -> Option<&'static str> {
    if name == "codex" || name.starts_with("codex-") {
        Some("Codex")
    } else if matches!(name, "claude" | "claude-code") {
        Some("Claude")
    } else if matches!(name, "opencode" | "open-code") {
        Some("OpenCode")
    } else if name == "grok" || name.starts_with("grok-") {
        Some("Grok")
    } else {
        None
    }
}

fn program_name(program: &str) -> String {
    let basename = program.rsplit(['/', '\\']).next().unwrap_or(program);
    normalize_program_name(basename)
}

fn program_name_os(program: &OsStr) -> String {
    let basename = std::path::Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy();
    normalize_program_name(&basename)
}

fn normalize_program_name(basename: &str) -> String {
    let basename = basename.strip_prefix('-').unwrap_or(basename);
    for suffix in [".exe", ".js", ".mjs", ".cjs", ".py"] {
        if let Some(name) = basename.strip_suffix(suffix) {
            return name.to_ascii_lowercase();
        }
    }
    basename.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_provider_process_is_not_detected() {
        assert!(detect("unsupported-agent", "project", "").is_none());
        assert!(agent_for_argv(&[OsString::from("unsupported-agent")]).is_none());
    }

    #[test]
    fn codex_ready_is_idle() {
        let result = detect(
            "codex-aarch64-apple-darwin",
            "sample-project",
            "gpt-5.6-sol · Ready · Context 45% left",
        )
        .unwrap();
        assert_eq!(result.agent, "Codex");
        assert_eq!(result.state, AgentState::Idle);
    }

    #[test]
    fn codex_spinner_is_working() {
        let result = detect(
            "codex-aarch64-apple-darwin",
            "⠸ sample-project",
            "Working (1m 15s · esc to interrupt)",
        )
        .unwrap();
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.source, EvidenceSource::Title);
    }

    #[test]
    fn codex_goal_footer_reports_pursuing_elapsed_time() {
        let result = detect(
            "codex",
            "⠸ sample-project",
            "› Keep working\n\ngpt-5.6-sol · Working · Pursuing goal (18m 42s)",
        )
        .unwrap();
        assert_eq!(
            result.goal,
            Some(GoalInfo {
                state: GoalState::Pursuing,
                elapsed_seconds: 1_122,
                achievement_pending: false,
                achievement_observed_at_ms: 0,
            })
        );
    }

    #[test]
    fn codex_goal_footer_reports_achieved_elapsed_time() {
        let result = detect(
            "codex",
            "sample-project",
            "gpt-5.6-sol · Ready · Main [default]     Goal achieved (2h 12m)",
        )
        .unwrap();
        assert_eq!(
            result.goal,
            Some(GoalInfo {
                state: GoalState::Achieved,
                elapsed_seconds: 7_920,
                achievement_pending: false,
                achievement_observed_at_ms: 0,
            })
        );
    }

    #[test]
    fn stale_goal_text_outside_the_footer_is_ignored() {
        let result = detect(
            "codex",
            "sample-project",
            "Goal achieved (2h 12m)\nold transcript\n\n› New request\n\ngpt-5.6-sol · Ready",
        )
        .unwrap();
        assert_eq!(result.goal, None);
    }

    #[test]
    fn goal_footer_is_codex_specific() {
        let result = detect("claude", "project", "Goal achieved (2h 12m)\n❯ ").unwrap();
        assert_eq!(result.goal, None);
    }

    #[test]
    fn goal_duration_parser_rejects_non_duration_text() {
        assert_eq!(parse_goal_duration("1d 2h 3m 4s"), Some(93_784));
        assert_eq!(parse_goal_duration("/goal resume"), None);
        assert_eq!(parse_goal_duration("forever"), None);
    }

    #[test]
    fn claude_uses_its_provider_detector() {
        let result = detect("claude", "⠂ project", "").unwrap();
        assert_eq!(result.agent, "Claude");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(
            result
                .details
                .and_then(|details| details.detector)
                .as_deref(),
            Some("Claude")
        );
    }

    #[test]
    fn opencode_uses_its_provider_detector() {
        let result = detect(
            "opencode",
            "",
            "△ Permission required\n↑↓ select · enter confirm · esc dismiss",
        )
        .unwrap();
        assert_eq!(result.agent, "OpenCode");
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(
            result
                .details
                .and_then(|details| details.detector)
                .as_deref(),
            Some("OpenCode")
        );
    }

    #[test]
    fn title_only_spinner_is_title_evidence() {
        let result = detect("codex", "⠸ sample-project", "").unwrap();
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(result.source, EvidenceSource::Title);
    }

    #[test]
    fn action_required_title_wins_over_working() {
        let result = detect(
            "codex",
            "Action Required",
            "Action required. Allow this command? esc to interrupt",
        )
        .unwrap();
        assert_eq!(result.state, AgentState::Blocked);
    }

    #[test]
    fn claude_prompt_box_overrides_stale_approval_history() {
        let result = detect(
            "claude",
            "work",
            "Do you want to proceed?\n1. Yes\n2. No\n──────────\n❯ \n──────────\n? for shortcuts",
        )
        .unwrap();
        assert_eq!(result.state, AgentState::Idle);
    }

    #[test]
    fn claude_permission_form_overrides_stale_idle_history() {
        let result = detect(
            "claude",
            "work",
            "❯ previous request\n──────────\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel",
        )
        .unwrap();
        assert_eq!(result.state, AgentState::Blocked);
    }

    #[test]
    fn ordinary_shell_is_not_an_agent() {
        assert!(detect("zsh", "notes", "$ ls",).is_none());
    }

    #[test]
    fn stale_title_and_screen_do_not_create_an_agent() {
        assert!(
            detect(
                "/bin/zsh",
                "grok",
                "Grok\nResume this session with grok --resume abc\n$",
            )
            .is_none()
        );
    }

    #[test]
    fn agent_name_in_an_argument_does_not_create_an_agent() {
        assert!(detect("/usr/bin/rg codex src", "search", "codex").is_none());
    }

    #[test]
    fn direct_agent_executable_is_detected() {
        let result = detect("/opt/homebrew/bin/grok", "work", "Ask anything").unwrap();
        assert_eq!(result.agent, "Grok");
    }

    #[test]
    fn runtime_agent_entrypoint_is_detected() {
        let result = detect(
            "/opt/homebrew/bin/node /opt/tools/codex.js",
            "work",
            "Type a message",
        )
        .unwrap();
        assert_eq!(result.agent, "Codex");
    }

    #[test]
    fn agent_named_runtime_argument_is_not_an_entrypoint() {
        assert!(detect("python build.py codex.py", "", "").is_none());
        assert!(detect("node runner.js /tmp/codex.js", "", "").is_none());
    }

    #[test]
    fn env_wrapper_recurses_into_runtime_entrypoint() {
        let result = detect("env MODE=dev node /opt/tools/codex.js", "", "").unwrap();
        assert_eq!(result.agent, "Codex");
    }

    #[test]
    fn argv_detection_preserves_executable_paths_with_spaces() {
        let command = [OsString::from("/tmp/my agents/codex")];
        assert_eq!(agent_for_argv(&command).as_deref(), Some("Codex"));
    }

    #[test]
    fn argv_detection_preserves_env_assignment_boundaries() {
        let command = [
            OsString::from("env"),
            OsString::from("PROMPT=work on codex later"),
            OsString::from("/opt/tools/claude"),
        ];
        assert_eq!(agent_for_argv(&command).as_deref(), Some("Claude"));
    }

    #[test]
    fn argv_detection_skips_env_option_operands() {
        let unset = [
            OsString::from("env"),
            OsString::from("-u"),
            OsString::from("FOO"),
            OsString::from("codex"),
        ];
        assert_eq!(agent_for_argv(&unset).as_deref(), Some("Codex"));

        let chdir = [
            OsString::from("env"),
            OsString::from("-C"),
            OsString::from("/tmp"),
            OsString::from("claude"),
        ];
        assert_eq!(agent_for_argv(&chdir).as_deref(), Some("Claude"));
    }
}
