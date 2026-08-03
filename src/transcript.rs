use crate::codex::{collect_rollouts, normalize_name, read_metadata};
use crate::model::{AgentRecord, SubagentInfo};
use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const MAX_EVENT_TEXT: usize = 8_000;
const MAX_RENDERED_LINES: usize = 5_000;
const MATCH_WINDOW_MS: u64 = 120_000;

pub fn run(record: &AgentRecord) -> Result<()> {
    let subagent = record
        .subagent
        .as_ref()
        .context("the selected record is not a subagent")?;
    if !record.agent.eq_ignore_ascii_case("codex") {
        bail!("read-only subagent viewing currently supports Codex");
    }
    let rollout = resolve_rollout(record, subagent)?;
    let title = subagent.name.as_deref().unwrap_or("agent").to_string();
    let mut terminal = ratatui::init();
    let result = run_loop(
        &mut terminal,
        &rollout,
        &title,
        subagent.finished_at_ms.is_some(),
    );
    ratatui::restore();
    result
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    rollout: &Path,
    title: &str,
    initially_finished: bool,
) -> Result<()> {
    let mut transcript = TranscriptTail::open(rollout)?;
    let mut scroll = transcript.lines.len().saturating_sub(1);
    let mut follow = true;
    let mut last_refresh = Instant::now();
    let mut finished = initially_finished || transcript.finished;

    loop {
        terminal.draw(|frame| render(frame, title, &transcript.lines, scroll, follow, finished))?;
        let wait = REFRESH_INTERVAL.saturating_sub(last_refresh.elapsed());
        if event::poll(wait).context("poll transcript input")?
            && let Event::Key(key) = event::read().context("read transcript input")?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('j') | KeyCode::Down => {
                    scroll = scroll.saturating_add(1);
                    follow = false;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    scroll = scroll.saturating_sub(1);
                    follow = false;
                }
                KeyCode::PageDown => {
                    scroll = scroll.saturating_add(12);
                    follow = false;
                }
                KeyCode::PageUp => {
                    scroll = scroll.saturating_sub(12);
                    follow = false;
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    scroll = 0;
                    follow = false;
                }
                KeyCode::Char('G') | KeyCode::End => {
                    scroll = transcript.lines.len().saturating_sub(1);
                    follow = true;
                }
                _ => {}
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            match transcript.read_appended() {
                Ok(changed) => {
                    finished |= transcript.finished;
                    if changed && follow {
                        scroll = transcript.lines.len().saturating_sub(1);
                    } else if changed {
                        scroll = scroll.min(transcript.lines.len().saturating_sub(1));
                    }
                }
                Err(error) => transcript.lines.push(ViewLine::error(format!("{error:#}"))),
            }
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn render(
    frame: &mut Frame,
    title: &str,
    lines: &[ViewLine],
    scroll: usize,
    follow: bool,
    finished: bool,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let status = if finished { "done" } else { "running" };
    let status_color = if finished { Color::Green } else { Color::Cyan };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " READ ONLY ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("subagent: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            clean_text(title),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {status}"), Style::default().fg(status_color)),
    ]))
    .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, chunks[0]);

    let visible_height = usize::from(chunks[1].height.max(1));
    let max_scroll = lines.len().saturating_sub(visible_height);
    let effective_scroll = if follow {
        max_scroll
    } else {
        scroll.min(max_scroll)
    };
    let waiting = [ViewLine {
        kind: ViewKind::Output,
        text: "Waiting for visible agent activity...".to_string(),
    }];
    let rendered_lines = if lines.is_empty() { &waiting } else { lines };
    let body = rendered_lines
        .iter()
        .map(ViewLine::render)
        .collect::<Vec<Line<'static>>>();
    frame.render_widget(
        Paragraph::new(body)
            .scroll((effective_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );

    let footer = if follow {
        "j/k scroll  g/G top/follow  q close  live follow"
    } else {
        "j/k scroll  g/G top/follow  q close  paused"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

struct TranscriptTail {
    reader: BufReader<File>,
    pending: String,
    lines: Vec<ViewLine>,
    calls: HashMap<String, String>,
    format_errors: HashSet<String>,
    finished: bool,
}

impl TranscriptTail {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open rollout {}", path.display()))?;
        let mut transcript = Self {
            reader: BufReader::new(file),
            pending: String::new(),
            lines: Vec::new(),
            calls: HashMap::new(),
            format_errors: HashSet::new(),
            finished: false,
        };
        transcript.read_appended()?;
        Ok(transcript)
    }

    fn read_appended(&mut self) -> Result<bool> {
        let mut changed = false;
        loop {
            let mut appended = String::new();
            let read = self
                .reader
                .read_line(&mut appended)
                .context("read Codex rollout")?;
            if read == 0 {
                break;
            }
            changed = true;
            self.pending.push_str(&appended);
            if !self.pending.ends_with('\n') {
                continue;
            }
            let raw = std::mem::take(&mut self.pending);
            let Ok(event) = serde_json::from_str::<Value>(raw.trim_end()) else {
                self.push_format_error(
                    "invalid_json",
                    "Codex rollout contains invalid JSONL; update tmux-agent or inspect the rollout format",
                );
                continue;
            };
            match event.get("type").and_then(Value::as_str) {
                Some("response_item") => {
                    if let Some((key, message)) =
                        render_response_item(&event["payload"], &mut self.calls, &mut self.lines)
                    {
                        self.push_format_error(&key, &message);
                    }
                }
                Some("event_msg") if event["payload"]["type"].as_str() == Some("task_complete") => {
                    self.finished = true;
                }
                _ => {}
            }
        }
        if self.lines.len() > MAX_RENDERED_LINES {
            self.lines.drain(0..self.lines.len() - MAX_RENDERED_LINES);
        }
        Ok(changed)
    }

    fn push_format_error(&mut self, key: &str, message: &str) {
        if self.format_errors.insert(key.to_string()) {
            self.lines.push(ViewLine::error(message.to_string()));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ViewKind {
    Assistant,
    Tool,
    Output,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewLine {
    kind: ViewKind,
    text: String,
}

impl ViewLine {
    fn error(text: String) -> Self {
        Self {
            kind: ViewKind::Error,
            text,
        }
    }

    fn render(&self) -> Line<'static> {
        let (label, color) = match self.kind {
            ViewKind::Assistant => ("assistant", Color::Cyan),
            ViewKind::Tool => ("tool", Color::Yellow),
            ViewKind::Output => ("output", Color::DarkGray),
            ViewKind::Error => ("error", Color::Red),
        };
        Line::from(vec![
            Span::styled(
                format!("{label:>9}  "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.text.clone()),
        ])
    }
}

fn render_response_item(
    payload: &Value,
    calls: &mut HashMap<String, String>,
    rendered: &mut Vec<ViewLine>,
) -> Option<(String, String)> {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") if payload.get("role").and_then(Value::as_str) == Some("assistant") => {
            let Some(content) = payload.get("content").and_then(Value::as_array) else {
                return Some((
                    "assistant_content".to_string(),
                    "unsupported Codex assistant message format; update tmux-agent".to_string(),
                ));
            };
            for item in content {
                match item.get("type").and_then(Value::as_str) {
                    Some("output_text") => {
                        let Some(text) = item.get("text").and_then(Value::as_str) else {
                            return Some((
                                "assistant_output_text".to_string(),
                                "unsupported Codex assistant text format; update tmux-agent"
                                    .to_string(),
                            ));
                        };
                        push_text(rendered, ViewKind::Assistant, text);
                    }
                    Some(item_type) => {
                        let item_type = truncate_text(&clean_text(item_type), 80);
                        return Some((
                            format!("assistant_item:{item_type}"),
                            format!(
                                "unsupported Codex assistant content type {item_type:?}; update tmux-agent"
                            ),
                        ));
                    }
                    None => {
                        return Some((
                            "assistant_item_missing_type".to_string(),
                            "unsupported Codex assistant content without a type; update tmux-agent"
                                .to_string(),
                        ));
                    }
                }
            }
            None
        }
        Some("message") | Some("reasoning") => None,
        Some("function_call") | Some("custom_tool_call") | Some("tool_search_call") => {
            let name = payload
                .get("name")
                .or_else(|| payload.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                calls.insert(call_id.to_string(), name.to_string());
            }
            let detail = payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .map(summarize_arguments)
                .filter(|value| !value.is_empty());
            let text = detail
                .map(|detail| format!("{name}: {detail}"))
                .unwrap_or_else(|| name.to_string());
            push_text(rendered, ViewKind::Tool, &text);
            None
        }
        Some("function_call_output")
        | Some("custom_tool_call_output")
        | Some("tool_search_output") => {
            let name = payload
                .get("call_id")
                .and_then(Value::as_str)
                .and_then(|call_id| calls.get(call_id))
                .map(String::as_str)
                .unwrap_or("tool");
            if let Some(output) = payload.get("output") {
                let output = extract_output(output);
                push_text(rendered, ViewKind::Output, &format!("{name}: {output}"));
                None
            } else {
                Some((
                    "tool_output".to_string(),
                    "unsupported Codex tool output format; update tmux-agent".to_string(),
                ))
            }
        }
        Some(item_type) => {
            let item_type = truncate_text(&clean_text(item_type), 80);
            Some((
                format!("response_item:{item_type}"),
                format!("unsupported Codex response item {item_type:?}; update tmux-agent"),
            ))
        }
        None => Some((
            "response_item_missing_type".to_string(),
            "unsupported Codex response item without a type; update tmux-agent".to_string(),
        )),
    }
}

fn summarize_arguments(value: &Value) -> String {
    let parsed = value
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| value.clone());
    for key in [
        "cmd",
        "message",
        "query",
        "target",
        "path",
        "task_name",
        "prompt",
    ] {
        if let Some(value) = parsed.get(key).and_then(Value::as_str) {
            return truncate_text(&clean_text(value), 400);
        }
    }
    truncate_text(&clean_text(&parsed.to_string()), 400)
}

fn extract_output(value: &Value) -> String {
    if let Some(raw) = value.as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw)
            && let Some(output) = parsed.get("output").and_then(Value::as_str)
        {
            return truncate_text(&clean_text(output), MAX_EVENT_TEXT);
        }
        return truncate_text(&clean_text(raw), MAX_EVENT_TEXT);
    }
    truncate_text(&clean_text(&value.to_string()), MAX_EVENT_TEXT)
}

fn push_text(lines: &mut Vec<ViewLine>, kind: ViewKind, text: &str) {
    let clean = truncate_text(&clean_text(text), MAX_EVENT_TEXT);
    for (index, line) in clean.lines().enumerate() {
        lines.push(ViewLine {
            kind: if index == 0 {
                kind.clone()
            } else {
                ViewKind::Output
            },
            text: line.to_string(),
        });
    }
}

fn clean_text(value: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let ansi = ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap());
    ansi.replace_all(value, "")
        .chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            character if character.is_control() => ' ',
            character => character,
        })
        .collect()
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn resolve_rollout(record: &AgentRecord, subagent: &SubagentInfo) -> Result<PathBuf> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .context("cannot determine CODEX_HOME")?;
    resolve_rollout_in(&codex_home.join("sessions"), record, subagent)
}

fn resolve_rollout_in(
    sessions: &Path,
    record: &AgentRecord,
    subagent: &SubagentInfo,
) -> Result<PathBuf> {
    let mut files = Vec::new();
    collect_rollouts(sessions, &mut files)?;
    if let Some(thread_id) = subagent.thread_id.as_deref() {
        let mut exact = files
            .iter()
            .filter_map(|path| read_metadata(path).ok())
            .filter(|metadata| metadata.thread_id.as_deref() == Some(thread_id));
        let rollout = exact
            .next()
            .with_context(|| format!("no Codex rollout matches subagent thread {thread_id}"))?;
        if exact.next().is_some() {
            bail!("multiple Codex rollouts have thread ID {thread_id}; refusing to guess");
        }
        return Ok(rollout.path);
    }
    let expected_name = subagent.name.as_deref().map(normalize_name);
    let mut candidates = files
        .into_iter()
        .filter_map(|path| read_metadata(&path).ok())
        .filter(|metadata| metadata.cwd == record.cwd)
        .filter_map(|metadata| {
            let delta = metadata.started_at_ms.abs_diff(subagent.started_at_ms);
            if delta > MATCH_WINDOW_MS {
                return None;
            }
            let candidate_name = metadata.name.as_deref().map(normalize_name);
            if expected_name.is_some()
                && candidate_name.is_some()
                && expected_name != candidate_name
            {
                return None;
            }
            let source_score = usize::from(metadata.thread_source.as_deref() == Some("subagent"));
            let name_score =
                usize::from(expected_name.is_some() && expected_name == candidate_name);
            Some(((source_score, name_score, u64::MAX - delta), metadata))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let Some((best_score, best)) = candidates.first() else {
        bail!(
            "no Codex rollout matches subagent {} in {}",
            subagent.name.as_deref().unwrap_or("agent"),
            record.cwd
        );
    };
    if candidates
        .get(1)
        .is_some_and(|(score, _)| score == best_score)
    {
        bail!("multiple Codex rollouts match this subagent; refusing to guess");
    }
    Ok(best.path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::parse_rfc3339_ms;
    use crate::model::{AgentOrigin, AgentState, Attention, EvidenceSource, SubagentInfo};
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    fn record(cwd: &str, started_at_ms: u64) -> (AgentRecord, SubagentInfo) {
        let subagent = SubagentInfo {
            parent_id: "local/default/parent".into(),
            started_at_ms,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        };
        (
            AgentRecord {
                id: "local/terminal/ttys001/10".into(),
                host: "local".into(),
                server: "default".into(),
                pane_id: "tty:ttys001".into(),
                pane_pid: 10,
                session_id: String::new(),
                session_name: String::new(),
                window_id: String::new(),
                window_index: 0,
                window_name: String::new(),
                pane_index: 0,
                agent: "Codex".into(),
                state: AgentState::Working,
                attention: Attention::Unknown,
                source: EvidenceSource::Process,
                title: "work".into(),
                label: None,
                cwd: cwd.into(),
                visible: true,
                seen: false,
                changed_at_ms: started_at_ms,
                origin: AgentOrigin::Terminal,
                terminal: Some("ttys001".into()),
                remote_alias: None,
                ssh_connection: None,
                focus_target: None,
                goal: None,
                subagent: Some(subagent.clone()),
                detection: None,
            },
            subagent,
        )
    }

    fn write_rollout(
        root: &Path,
        filename: &str,
        timestamp: &str,
        cwd: &str,
        thread_source: &str,
        source: Value,
    ) -> PathBuf {
        let path = root.join(filename);
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": timestamp,
                "type": "session_meta",
                "payload": {
                    "id": filename,
                    "timestamp": timestamp,
                    "cwd": cwd,
                    "thread_source": thread_source,
                    "source": source
                }
            })
        )
        .unwrap();
        path
    }

    #[test]
    fn parses_rfc3339_milliseconds_and_offsets() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_rfc3339_ms("2026-07-26T02:30:32.019Z").unwrap(),
            1_785_033_032_019
        );
        assert_eq!(
            parse_rfc3339_ms("2026-07-26T04:30:32.019+02:00").unwrap(),
            1_785_033_032_019
        );
    }

    #[test]
    fn resolver_prefers_named_subagent_thread_over_outer_exec() {
        let directory = tempdir().unwrap();
        let started = parse_rfc3339_ms("2026-07-26T02:30:32.019Z").unwrap();
        let outer = write_rollout(
            directory.path(),
            "rollout-outer.jsonl",
            "2026-07-26T02:30:32.019Z",
            "/work",
            "user",
            Value::String("exec".into()),
        );
        let inner = write_rollout(
            directory.path(),
            "rollout-inner.jsonl",
            "2026-07-26T02:30:32.030Z",
            "/work",
            "subagent",
            serde_json::json!({"subagent": "review"}),
        );
        let (record, subagent) = record("/work", started);

        let resolved = resolve_rollout_in(directory.path(), &record, &subagent).unwrap();

        assert_ne!(resolved, outer);
        assert_eq!(resolved, inner);
    }

    #[test]
    fn resolver_prefers_exact_thread_identity_over_heuristics() {
        let directory = tempdir().unwrap();
        let started = parse_rfc3339_ms("2026-07-26T02:30:32.019Z").unwrap();
        let exact = write_rollout(
            directory.path(),
            "rollout-exact-thread.jsonl",
            "2026-07-26T02:30:32.019Z",
            "/different-cwd",
            "subagent",
            serde_json::json!({"subagent": "different-name"}),
        );
        write_rollout(
            directory.path(),
            "rollout-heuristic-thread.jsonl",
            "2026-07-26T02:30:32.019Z",
            "/work",
            "subagent",
            serde_json::json!({"subagent": "review"}),
        );
        let (record, mut subagent) = record("/work", started);
        subagent.thread_id = Some("rollout-exact-thread.jsonl".into());

        let resolved = resolve_rollout_in(directory.path(), &record, &subagent).unwrap();

        assert_eq!(resolved, exact);
    }

    #[test]
    fn transcript_excludes_reasoning_and_user_messages() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-test.jsonl");
        let mut file = File::create(&path).unwrap();
        for event in [
            serde_json::json!({
                "type": "response_item",
                "payload": {"type":"reasoning","summary":[{"text":"private reasoning"}]}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {"type":"message","role":"user","content":[{"type":"input_text","text":"hidden prompt"}]}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Found the issue."}]}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {"type":"function_call","name":"exec_command","call_id":"1","arguments":"{\"cmd\":\"cargo test\"}"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {"type":"function_call_output","call_id":"1","output":"tests passed"}
            }),
        ] {
            writeln!(file, "{event}").unwrap();
        }
        drop(file);

        let transcript = TranscriptTail::open(&path).unwrap();
        let text = transcript
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("private reasoning"));
        assert!(!text.contains("hidden prompt"));
        assert!(text.contains("Found the issue."));
        assert!(text.contains("cargo test"));
        assert!(text.contains("tests passed"));
    }

    #[test]
    fn tail_reader_waits_for_complete_appended_json_lines() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-tail.jsonl");
        File::create(&path).unwrap();
        let mut transcript = TranscriptTail::open(&path).unwrap();
        let event = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "new result"}]
            }
        })
        .to_string();

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{event}").unwrap();
        file.flush().unwrap();
        assert!(transcript.read_appended().unwrap());
        assert!(transcript.lines.is_empty());

        writeln!(file).unwrap();
        file.flush().unwrap();
        assert!(transcript.read_appended().unwrap());
        assert_eq!(transcript.lines.len(), 1);
        assert_eq!(transcript.lines[0].text, "new result");
    }

    #[test]
    fn unknown_rollout_formats_fail_safely_without_rendering_payload_content() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-unknown.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "future_private_format",
                    "secret": "must not render"
                }
            })
        )
        .unwrap();
        writeln!(file, "not-json").unwrap();
        drop(file);

        let transcript = TranscriptTail::open(&path).unwrap();
        let text = transcript
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("unsupported Codex response item"));
        assert!(text.contains("invalid JSONL"));
        assert!(!text.contains("must not render"));
    }

    #[test]
    fn live_transcript_polling_interval_is_one_tenth_second() {
        assert_eq!(REFRESH_INTERVAL, Duration::from_millis(100));
    }
}
