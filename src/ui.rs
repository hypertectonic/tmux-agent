use crate::config::{Config, RuntimePaths, shell_join};
use crate::ipc;
use crate::model::{
    AgentOrigin, AgentRecord, AgentState, Attention, GoalInfo, GoalState, Snapshot, terminal_safe,
    trim_braille_activity_prefix,
};
use crate::tmux::{Tmux, is_focus_target_missing};
use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAME_TIME: Duration = Duration::from_millis(120);
const ACTION_MESSAGE_DURATION: Duration = Duration::from_secs(3);
const PROVIDER_WIDTH: usize = 8;

pub async fn run(
    paths: &RuntimePaths,
    tmux: Tmux,
    popup: bool,
    config: &Config,
    config_path: &Path,
) -> Result<()> {
    let pane_id = (!popup).then(|| std::env::var("TMUX_PANE").ok()).flatten();
    if let Some(pane_id) = &pane_id {
        tmux.set_ui_marker(pane_id, true)?;
    }
    let _guard = UiMarkerGuard {
        tmux: tmux.clone(),
        pane_id,
    };
    let mut terminal = ratatui::init();
    if let Err(error) = execute!(io::stdout(), EnableMouseCapture) {
        ratatui::restore();
        return Err(error).context("enable mouse capture");
    }
    let result = run_loop(&mut terminal, paths, &tmux, popup, config, config_path).await;
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    match result? {
        LoopExit::Close => Ok(()),
        LoopExit::RunInCurrentTerminal(command) => run_in_current_terminal(&command),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LoopExit {
    Close,
    RunInCurrentTerminal(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderTopology {
    has_peers: bool,
    rows: Vec<(String, usize)>,
}

impl RenderTopology {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            has_peers: !snapshot.peers.is_empty(),
            rows: snapshot
                .agents
                .iter()
                .map(|agent| (agent.id.clone(), agent_row_height(agent)))
                .collect(),
        }
    }
}

#[derive(Debug, Default)]
struct RedrawTracker {
    rendered: Option<RenderTopology>,
    forced: bool,
}

impl RedrawTracker {
    fn needs_full_redraw(&self, topology: &RenderTopology) -> bool {
        self.forced || self.rendered.as_ref() != Some(topology)
    }

    fn force(&mut self) {
        self.forced = true;
    }

    fn mark_rendered(&mut self, topology: RenderTopology) {
        self.rendered = Some(topology);
        self.forced = false;
    }
}

#[derive(Debug, Default)]
enum UiMessage {
    #[default]
    None,
    Transient {
        text: String,
        expires_at: Instant,
    },
    DaemonError(String),
}

impl UiMessage {
    fn set_transient(&mut self, text: impl Into<String>, now: Instant) {
        *self = Self::Transient {
            text: text.into(),
            expires_at: now + ACTION_MESSAGE_DURATION,
        };
    }

    fn set_daemon_error(&mut self, text: impl Into<String>) {
        *self = Self::DaemonError(text.into());
    }

    fn clear_daemon_error(&mut self) {
        if matches!(self, Self::DaemonError(_)) {
            *self = Self::None;
        }
    }

    fn expire(&mut self, now: Instant) {
        if matches!(self, Self::Transient { expires_at, .. } if now >= *expires_at) {
            *self = Self::None;
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::None => "",
            Self::Transient { text, .. } | Self::DaemonError(text) => text,
        }
    }
}

async fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    paths: &RuntimePaths,
    tmux: &Tmux,
    exit_after_focus: bool,
    config: &Config,
    config_path: &Path,
) -> Result<LoopExit> {
    let activation_context = ActivationContext {
        paths,
        tmux,
        config,
        config_path,
        exit_after_focus,
    };
    let mut snapshot = ipc::snapshot(&paths.socket, false).await?;
    let mut selected = 0usize;
    let mut message = UiMessage::default();
    let mut last_refresh = Instant::now();
    let animation_started = Instant::now();
    let mut redraw = RedrawTracker::default();

    loop {
        message.expire(Instant::now());
        if selected >= snapshot.agents.len() && !snapshot.agents.is_empty() {
            selected = snapshot.agents.len() - 1;
        }
        let spinner_frame = spinner_frame(animation_started.elapsed());
        let topology = RenderTopology::from_snapshot(&snapshot);
        if redraw.needs_full_redraw(&topology) {
            terminal.clear().context("clear terminal for full redraw")?;
        }
        terminal.draw(|frame| render(frame, &snapshot, selected, message.text(), spinner_frame))?;
        redraw.mark_rendered(topology);
        if event::poll(Duration::from_millis(100)).context("poll terminal input")? {
            match event::read().context("read terminal input")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(LoopExit::Close),
                    KeyCode::Char('j') | KeyCode::Down => {
                        if selected + 1 < snapshot.agents.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Char('g') | KeyCode::Home => selected = 0,
                    KeyCode::Char('G') | KeyCode::End => {
                        selected = snapshot.agents.len().saturating_sub(1)
                    }
                    KeyCode::Enter => {
                        match activate_record(
                            &activation_context,
                            &mut snapshot,
                            selected,
                            &mut message,
                        )
                        .await?
                        {
                            Activation::Continue => {}
                            Activation::Close => return Ok(LoopExit::Close),
                            Activation::RunInCurrentTerminal(command) => {
                                return Ok(LoopExit::RunInCurrentTerminal(command));
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        snapshot = ipc::snapshot(&paths.socket, false).await?;
                        last_refresh = Instant::now();
                        message.set_transient("refreshed", Instant::now());
                        redraw.force();
                    }
                    _ => {}
                },
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    let (width, height) =
                        crossterm::terminal::size().context("read terminal size")?;
                    if let Some(index) = agent_at_mouse(
                        Rect::new(0, 0, width, height),
                        !snapshot.peers.is_empty(),
                        selected,
                        &snapshot.agents,
                        mouse.column,
                        mouse.row,
                    ) {
                        selected = index;
                        match activate_record(
                            &activation_context,
                            &mut snapshot,
                            selected,
                            &mut message,
                        )
                        .await?
                        {
                            Activation::Continue => {}
                            Activation::Close => return Ok(LoopExit::Close),
                            Activation::RunInCurrentTerminal(command) => {
                                return Ok(LoopExit::RunInCurrentTerminal(command));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if last_refresh.elapsed() >= Duration::from_millis(500) {
            apply_daemon_refresh(
                &mut snapshot,
                &mut message,
                ipc::snapshot(&paths.socket, false).await,
            );
            last_refresh = Instant::now();
        }
    }
}

fn apply_daemon_refresh(
    snapshot: &mut Snapshot,
    message: &mut UiMessage,
    result: Result<Snapshot>,
) {
    match result {
        Ok(next) => {
            *snapshot = next;
            message.clear_daemon_error();
        }
        Err(error) => message.set_daemon_error(format!("daemon: {error:#}")),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Activation {
    Continue,
    Close,
    RunInCurrentTerminal(Vec<String>),
}

struct ActivationContext<'a> {
    paths: &'a RuntimePaths,
    tmux: &'a Tmux,
    config: &'a Config,
    config_path: &'a Path,
    exit_after_focus: bool,
}

async fn activate_record(
    context: &ActivationContext<'_>,
    snapshot: &mut Snapshot,
    selected: usize,
    message: &mut UiMessage,
) -> Result<Activation> {
    let Some(record) = snapshot.agents.get(selected).cloned() else {
        return Ok(Activation::Continue);
    };
    if record.subagent.is_some() {
        let command =
            subagent_view_command(context.config, context.config_path, snapshot, &record)?;
        if context.exit_after_focus {
            return Ok(Activation::RunInCurrentTerminal(command));
        }
        match context.tmux.display_popup(&shell_join(&command)) {
            Ok(()) => message.set_transient("opened read-only subagent view", Instant::now()),
            Err(error) => message.set_transient(format!("{error:#}"), Instant::now()),
        }
        return Ok(Activation::Continue);
    }
    let focus_record = &record;
    if focus_record.is_tmux() || focus_record.remote_alias.is_some() {
        return match context.tmux.focus_agent(focus_record) {
            Ok(()) => {
                if activation_requires_acknowledgement(focus_record) {
                    acknowledge_record(context.paths, snapshot, &record.id).await?;
                }
                if context.exit_after_focus {
                    return Ok(Activation::Close);
                }
                message.set_transient(
                    format!("focused {}", focus_record.location()),
                    Instant::now(),
                );
                Ok(Activation::Continue)
            }
            Err(focus_error)
                if focus_record.remote_alias.is_some()
                    && (record.attention == Attention::Done
                        || has_pending_goal_achievement(focus_record))
                    && is_focus_target_missing(&focus_error) =>
            {
                acknowledge_record(context.paths, snapshot, &record.id).await?;
                message.set_transient(
                    format!(
                        "acknowledged {}; focus unavailable: {focus_error:#}",
                        record.location()
                    ),
                    Instant::now(),
                );
                Ok(Activation::Continue)
            }
            Err(error) => {
                message.set_transient(format!("{error:#}"), Instant::now());
                Ok(Activation::Continue)
            }
        };
    }
    match acknowledge_record(context.paths, snapshot, &record.id).await {
        Ok(()) => message.set_transient(
            format!("acknowledged {}", record.location()),
            Instant::now(),
        ),
        Err(error) => message.set_transient(format!("{error:#}"), Instant::now()),
    }
    Ok(Activation::Continue)
}

fn activation_requires_acknowledgement(record: &AgentRecord) -> bool {
    record.attention == Attention::Done || has_pending_goal_achievement(record)
}

fn has_pending_goal_achievement(record: &AgentRecord) -> bool {
    !matches!(record.state, AgentState::Working | AgentState::Blocked)
        && record
            .goal
            .is_some_and(|goal| goal.state == GoalState::Achieved && goal.achievement_pending)
}

fn subagent_view_command(
    config: &Config,
    config_path: &Path,
    snapshot: &Snapshot,
    record: &AgentRecord,
) -> Result<Vec<String>> {
    if let Some(alias) = record.remote_alias.as_deref() {
        let machine = config.machine(alias).with_context(|| {
            format!(
                "remote {alias:?} uses a raw collector command; add a structured [[machine]] entry to open subagent views"
            )
        })?;
        let peer = snapshot
            .peers
            .iter()
            .find(|peer| peer.name == alias)
            .with_context(|| format!("remote {alias:?} has no peer capability information"))?;
        if !peer
            .capabilities
            .iter()
            .any(|capability| capability == crate::model::CAPABILITY_SUBAGENT_VIEW)
        {
            let version = peer.application_version.as_deref().unwrap_or("unknown");
            anyhow::bail!(
                "remote {alias:?} runs tmux-agent {version} without {}. Update it to tmux-agent {} or newer",
                crate::model::CAPABILITY_SUBAGENT_VIEW,
                crate::model::SUBAGENT_VIEW_MINIMUM_VERSION
            );
        }
        let prefix = format!("remote/{alias}/");
        let target = record.id.strip_prefix(&prefix).with_context(|| {
            format!("remote subagent ID does not begin with expected namespace {prefix:?}")
        })?;
        return Ok(machine.subagent_view_command(target));
    }

    let executable = std::env::current_exe().context("resolve tmux-agent executable")?;
    Ok(vec![
        executable.to_string_lossy().into_owned(),
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "subagent-view".to_string(),
        "--local-only".to_string(),
        record.id.clone(),
    ])
}

fn run_in_current_terminal(command: &[String]) -> Result<()> {
    let (program, arguments) = command
        .split_first()
        .context("subagent viewer command is empty")?;
    let status = Command::new(program)
        .args(arguments)
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        anyhow::bail!("subagent viewer exited with {status}");
    }
    Ok(())
}

async fn acknowledge_record(
    paths: &RuntimePaths,
    snapshot: &mut Snapshot,
    record_id: &str,
) -> Result<()> {
    ipc::acknowledge(&paths.socket, record_id).await?;
    *snapshot = ipc::snapshot(&paths.socket, false).await?;
    Ok(())
}

fn render(
    frame: &mut Frame,
    snapshot: &Snapshot,
    selected: usize,
    message: &str,
    spinner_frame: usize,
) {
    let chunks = ui_layout(frame.area(), !snapshot.peers.is_empty());

    let counts = snapshot
        .agents
        .iter()
        .filter(|agent| agent.subagent.is_none())
        .fold([0usize; 5], |mut counts, agent| {
            counts[agent.attention.rank() as usize] += 1;
            counts
        });
    let active_subagents = snapshot
        .agents
        .iter()
        .filter_map(|agent| {
            agent
                .subagent
                .as_ref()
                .filter(|subagent| subagent.finished_at_ms.is_none())
                .map(|subagent| subagent.parent_id.as_str())
        })
        .fold(HashMap::<&str, usize>::new(), |mut counts, parent_id| {
            *counts.entry(parent_id).or_default() += 1;
            counts
        });
    let subagent_depths = subagent_depths(&snapshot.agents);
    let header = Paragraph::new(vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                " tmux-agent ",
                Style::default()
                    .fg(Color::Rgb(150, 205, 200))
                    .bg(Color::Rgb(35, 55, 58)),
            ),
            Span::raw(" "),
            Span::styled(
                format!("!{}", counts[0]),
                Style::default().fg(attention_color(Attention::Blocked)),
            ),
            Span::raw("  "),
            Span::styled(
                format!("✓{}", counts[1]),
                Style::default().fg(attention_color(Attention::Done)),
            ),
            Span::raw("  "),
            Span::styled(
                format!("●{}", counts[2]),
                Style::default().fg(attention_color(Attention::Working)),
            ),
        ]),
    ])
    .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, chunks[0]);

    let list_width = chunks[1].width.saturating_sub(1) as usize;
    let items = snapshot
        .agents
        .iter()
        .enumerate()
        .map(|(index, agent)| {
            if let Some(subagent) = &agent.subagent {
                let finished = subagent.finished_at_ms.is_some();
                let state_color = if finished { Color::Green } else { Color::Cyan };
                let state_label = if finished { "done" } else { "running" };
                let end = subagent
                    .finished_at_ms
                    .unwrap_or(snapshot.generated_at_ms.max(subagent.started_at_ms));
                let duration = format_duration(
                    end.saturating_sub(subagent.started_at_ms)
                        .saturating_div(1_000),
                );
                let name = subagent.name.as_deref().unwrap_or("agent");
                let depth = subagent_depths.get(&agent.id).copied().unwrap_or(1);
                return ListItem::new(Line::from(vec![
                    Span::raw(" ".repeat(2 + PROVIDER_WIDTH + 2 + depth.saturating_sub(1) * 2)),
                    Span::styled("↳ ", Style::default().fg(Color::Yellow)),
                    Span::styled("subagent: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        terminal_safe(name),
                        Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {state_label}"), Style::default().fg(state_color)),
                    Span::styled(
                        format!("  ·  {duration}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
                .style(Style::default());
            }

            let color = attention_color(agent.attention);
            let title = display_title(agent);
            let safe_title = terminal_safe(&title);
            let safe_location = terminal_safe(&location_breadcrumb(agent));
            let state_label = attention_label(agent.attention);
            let show_state = list_width >= 46;
            let goal_label = show_state
                .then(|| {
                    agent
                        .goal
                        .as_ref()
                        .filter(|goal| {
                            goal.state == GoalState::Pursuing || has_pending_goal_achievement(agent)
                        })
                        .map(|goal| goal_label(goal, list_width < 60))
                })
                .flatten();
            let child_count = active_subagents
                .get(agent.id.as_str())
                .copied()
                .filter(|count| *count > 0)
                .map(|count| {
                    if count == 1 {
                        "+1 agent".to_string()
                    } else {
                        format!("+{count} agents")
                    }
                })
                .filter(|_| show_state);
            let state_width = if show_state {
                state_label.chars().count() + 2
            } else {
                0
            };
            let goal_width = goal_label
                .as_ref()
                .map(|label| label.chars().count() + 2)
                .unwrap_or(0);
            let child_count_width = child_count
                .as_ref()
                .map(|label| label.chars().count() + 2)
                .unwrap_or(0);
            let title_width = list_width
                .saturating_sub(
                    2 + PROVIDER_WIDTH + 2 + state_width + goal_width + child_count_width,
                )
                .max(1);
            let location_width = list_width.saturating_sub(2 + PROVIDER_WIDTH + 2).max(1);
            let (provider, provider_style) = provider_badge(&agent.agent);
            let row_style = agent_row_style(agent.attention, index == selected);
            let glyph = attention_glyph(agent.attention, spinner_frame);
            let glyph_color = if agent.attention == Attention::Working {
                provider_style.fg.unwrap_or(color)
            } else {
                color
            };

            let mut first_line = vec![
                Span::styled(
                    format!("{glyph} "),
                    Style::default()
                        .fg(glyph_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(provider, provider_style),
                Span::raw("  "),
                Span::styled(
                    truncate(&safe_title, title_width),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ];
            if show_state {
                first_line.push(Span::styled(
                    format!("  {state_label}"),
                    Style::default().fg(color),
                ));
            }
            if let Some(goal_label) = goal_label {
                first_line.push(Span::styled(
                    format!("  {goal_label}"),
                    Style::default().fg(Color::Magenta),
                ));
            }
            if let Some(child_count) = child_count {
                first_line.push(Span::styled(
                    format!("  {child_count}"),
                    Style::default().fg(Color::Gray),
                ));
            }

            ListItem::new(vec![
                Line::from(first_line),
                Line::from(vec![
                    Span::raw(" ".repeat(2 + PROVIDER_WIDTH + 2)),
                    Span::styled(
                        truncate(&safe_location, location_width),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
                .style(Style::default().fg(Color::DarkGray)),
            ])
            .style(row_style)
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(Line::from(Span::styled(
            "▌",
            Style::default().fg(Color::Cyan),
        )))
        .highlight_spacing(HighlightSpacing::Always)
        .repeat_highlight_symbol(true);
    let mut list_state = ListState::default();
    if !snapshot.agents.is_empty() {
        list_state.select(Some(selected));
    }
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    if !snapshot.peers.is_empty() {
        let mut peers = Vec::new();
        for (index, peer) in snapshot.peers.iter().enumerate() {
            if index > 0 {
                peers.push(Span::raw("  "));
            }
            let color = if peer.connected {
                Color::Green
            } else {
                Color::Red
            };
            peers.push(Span::styled("● ", Style::default().fg(color)));
            let label = if peer.connected {
                format!(
                    "{} v{}",
                    terminal_safe(&peer.name),
                    peer.application_version.as_deref().unwrap_or("?")
                )
            } else {
                format!(
                    "{} ({})",
                    terminal_safe(&peer.name),
                    terminal_safe(
                        peer.last_error
                            .as_deref()
                            .unwrap_or("remote collector disconnected")
                    )
                )
            };
            peers.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
        }
        frame.render_widget(Paragraph::new(Line::from(peers)), chunks[2]);
    }
    let safe_message = terminal_safe(message);
    let footer = if message.is_empty() {
        "j/k move  enter focus/view  r refresh  q close"
    } else {
        safe_message.as_str()
    };
    frame.render_widget(
        Paragraph::new(truncate(
            footer,
            frame.area().width.saturating_sub(1) as usize,
        ))
        .style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
}

fn subagent_depths(agents: &[AgentRecord]) -> HashMap<String, usize> {
    let parents = agents
        .iter()
        .filter_map(|agent| {
            agent
                .subagent
                .as_ref()
                .map(|subagent| (agent.id.as_str(), subagent.parent_id.as_str()))
        })
        .collect::<HashMap<_, _>>();
    agents
        .iter()
        .filter(|agent| agent.subagent.is_some())
        .map(|agent| {
            let mut current = agent.id.as_str();
            let mut depth = 0;
            let mut visited = HashSet::new();
            while let Some(parent) = parents.get(current).copied() {
                if !visited.insert(current) {
                    break;
                }
                depth += 1;
                current = parent;
            }
            (agent.id.clone(), depth.max(1))
        })
        .collect()
}

fn ui_layout(area: Rect, has_peers: bool) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(if has_peers { 2 } else { 1 }),
            Constraint::Length(1),
        ])
        .split(area)
        .to_vec()
}

fn agent_row_height(agent: &AgentRecord) -> usize {
    if agent.subagent.is_some() { 1 } else { 2 }
}

fn agent_at_mouse(
    area: Rect,
    has_peers: bool,
    selected: usize,
    agents: &[AgentRecord],
    column: u16,
    row: u16,
) -> Option<usize> {
    let list = ui_layout(area, has_peers)[1];
    if column < list.x
        || column >= list.x.saturating_add(list.width)
        || row < list.y
        || row >= list.y.saturating_add(list.height)
    {
        return None;
    }
    let heights = agents.iter().map(agent_row_height).collect::<Vec<_>>();
    let offset = visible_list_offset(&heights, selected, usize::from(list.height));
    let mut relative_row = usize::from(row - list.y);
    let mut rendered_height = 0usize;
    for (index, height) in heights.into_iter().enumerate().skip(offset) {
        if rendered_height.saturating_add(height) > usize::from(list.height) {
            break;
        }
        if relative_row < height {
            return Some(index);
        }
        relative_row = relative_row.saturating_sub(height);
        rendered_height += height;
    }
    None
}

fn visible_list_offset(heights: &[usize], selected: usize, viewport_height: usize) -> usize {
    if heights.is_empty() || viewport_height == 0 {
        return 0;
    }
    let selected = selected.min(heights.len() - 1);
    let mut offset = selected;
    let mut occupied = heights[selected];
    while offset > 0 && occupied.saturating_add(heights[offset - 1]) <= viewport_height {
        offset -= 1;
        occupied += heights[offset];
    }
    offset
}

fn attention_color(attention: Attention) -> Color {
    match attention {
        Attention::Blocked => Color::Red,
        Attention::Done => Color::Green,
        Attention::Working => Color::Cyan,
        Attention::Idle => Color::DarkGray,
        Attention::Unknown => Color::Yellow,
    }
}

fn attention_glyph(attention: Attention, spinner_frame: usize) -> &'static str {
    if attention == Attention::Working {
        SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()]
    } else {
        attention.icon()
    }
}

fn attention_label(attention: Attention) -> &'static str {
    match attention {
        Attention::Blocked => "needs input",
        Attention::Done => "done",
        Attention::Working => "working",
        Attention::Idle => "idle",
        Attention::Unknown => "unknown",
    }
}

fn goal_label(goal: &GoalInfo, compact: bool) -> String {
    let state = match (goal.state, compact) {
        (GoalState::Pursuing, false) => "Pursuing goal",
        (GoalState::Achieved, false) => "Goal achieved",
        (GoalState::Pursuing, true) => "goal",
        (GoalState::Achieved, true) => "goal✓",
    };
    format!("{state} ({})", format_goal_duration(goal.elapsed_seconds))
}

fn format_duration(elapsed_seconds: u64) -> String {
    let days = elapsed_seconds / 86_400;
    let hours = elapsed_seconds % 86_400 / 3_600;
    let minutes = elapsed_seconds % 3_600 / 60;
    let seconds = elapsed_seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_goal_duration(elapsed_seconds: u64) -> String {
    format_duration(elapsed_seconds)
}

fn spinner_frame(elapsed: Duration) -> usize {
    let frame_time_ms = SPINNER_FRAME_TIME.as_millis().max(1);
    (elapsed.as_millis() / frame_time_ms) as usize % SPINNER_FRAMES.len()
}

fn provider_badge(agent: &str) -> (String, Style) {
    let (label, foreground, background) = provider_palette(agent);
    let label = truncate(&terminal_safe(label), PROVIDER_WIDTH);
    (
        format!("{label:<PROVIDER_WIDTH$}"),
        Style::default()
            .fg(foreground)
            .bg(background)
            .add_modifier(Modifier::BOLD),
    )
}

fn provider_palette(agent: &str) -> (&str, Color, Color) {
    let normalized = agent.to_ascii_lowercase();
    match normalized.as_str() {
        "codex" => ("CODEX", Color::Rgb(110, 230, 220), Color::Rgb(20, 60, 62)),
        "claude" => ("CLAUDE", Color::Rgb(240, 160, 90), Color::Rgb(70, 43, 24)),
        "opencode" => (
            "OPENCODE",
            Color::Rgb(190, 145, 255),
            Color::Rgb(50, 35, 75),
        ),
        "grok" => ("GROK", Color::Rgb(170, 195, 240), Color::Rgb(35, 45, 65)),
        "pi" => ("PI", Color::Rgb(205, 235, 125), Color::Rgb(48, 62, 28)),
        _ => (agent, Color::Gray, Color::Rgb(45, 45, 45)),
    }
}

fn agent_row_style(attention: Attention, selected: bool) -> Style {
    let background = match (attention, selected) {
        (Attention::Blocked, true) => Color::Rgb(65, 25, 30),
        (Attention::Blocked, false) => Color::Rgb(45, 20, 24),
        (_, true) => Color::Rgb(35, 45, 55),
        (_, false) => return Style::default(),
    };
    Style::default().bg(background)
}

fn location_breadcrumb(agent: &AgentRecord) -> String {
    if let Some(target) = &agent.focus_target {
        return format!(
            "{} › {} › {}.{}",
            agent.host, target.session_name, target.window_index, target.pane_index
        );
    }
    match agent.origin {
        AgentOrigin::Tmux => format!(
            "{} › {} › {}.{}",
            agent.host, agent.session_name, agent.window_index, agent.pane_index
        ),
        AgentOrigin::Terminal => format!(
            "{} › tty {}",
            agent.host,
            agent.terminal.as_deref().unwrap_or("unknown")
        ),
    }
}

fn display_title(agent: &AgentRecord) -> String {
    let stable_grok_title = if agent.agent.eq_ignore_ascii_case("grok") {
        Path::new(&agent.cwd)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .or_else(|| (!agent.cwd.is_empty()).then_some(agent.cwd.as_str()))
    } else {
        None
    };
    let title = stable_grok_title.unwrap_or_else(|| trim_braille_activity_prefix(&agent.title));
    let title = if title.is_empty() {
        trim_braille_activity_prefix(&agent.window_name)
    } else {
        title
    };
    let label = agent
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty());
    match (title.is_empty(), label) {
        (false, Some(label)) if label != title => format!("{title} | {label}"),
        (false, _) => title.to_string(),
        (true, Some(label)) => label.to_string(),
        (true, None) => String::new(),
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut result = value.chars().take(width - 1).collect::<String>();
    result.push('…');
    result
}

struct UiMarkerGuard {
    tmux: Tmux,
    pane_id: Option<String>,
}

impl Drop for UiMarkerGuard {
    fn drop(&mut self) {
        if let Some(pane_id) = &self.pane_id {
            let _ = self.tmux.set_ui_marker(pane_id, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentState, EvidenceSource, SubagentInfo};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn test_agent(agent: &str, attention: Attention, origin: AgentOrigin) -> AgentRecord {
        AgentRecord {
            id: format!("local/default/{agent}"),
            host: "remote-mac".into(),
            server: "default".into(),
            pane_id: "%1".into(),
            pane_pid: 10,
            session_id: "$1".into(),
            session_name: "project-one".into(),
            window_id: "@1".into(),
            window_index: 1,
            window_name: "work".into(),
            pane_index: 0,
            agent: agent.into(),
            state: AgentState::Idle,
            attention,
            source: EvidenceSource::Screen,
            title: "long-project-name".into(),
            label: None,
            cwd: "/work".into(),
            visible: true,
            seen: false,
            changed_at_ms: 0,
            origin,
            terminal: (origin == AgentOrigin::Terminal).then(|| "ttys005".into()),
            remote_alias: None,
            ssh_connection: None,
            focus_target: None,
            goal: None,
            subagent: None,
            detection: None,
        }
    }

    fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn header_starts_below_a_blank_row_and_uses_a_muted_badge() {
        let snapshot = Snapshot::default();
        let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 0))
            .unwrap();

        assert!(row_text(&terminal, 0).is_empty());
        let header = row_text(&terminal, 1);
        assert!(header.contains("tmux-agent"));
        let badge_x = header.find("tmux-agent").unwrap() as u16;
        let badge = &terminal.backend().buffer()[(badge_x, 1)];
        assert_eq!(badge.fg, Color::Rgb(150, 205, 200));
        assert_eq!(badge.bg, Color::Rgb(35, 55, 58));
    }

    #[test]
    fn successful_daemon_refresh_clears_only_daemon_errors() {
        let mut snapshot = Snapshot {
            revision: 1,
            ..Snapshot::default()
        };
        let mut message = UiMessage::default();
        message.set_daemon_error("daemon: connect to daemon socket");

        apply_daemon_refresh(
            &mut snapshot,
            &mut message,
            Ok(Snapshot {
                revision: 2,
                ..Snapshot::default()
            }),
        );

        assert_eq!(snapshot.revision, 2);
        assert!(message.text().is_empty());

        message.set_transient("focused project-one:1.0", Instant::now());
        apply_daemon_refresh(
            &mut snapshot,
            &mut message,
            Ok(Snapshot {
                revision: 3,
                ..Snapshot::default()
            }),
        );
        assert_eq!(message.text(), "focused project-one:1.0");

        apply_daemon_refresh(
            &mut snapshot,
            &mut message,
            Err(anyhow::anyhow!("socket unavailable")),
        );
        assert_eq!(snapshot.revision, 3);
        assert_eq!(message.text(), "daemon: socket unavailable");
    }

    #[test]
    fn transient_messages_expire_after_three_seconds() {
        let started = Instant::now();
        let mut message = UiMessage::default();
        message.set_transient("focused project-one:1.0", started);

        message.expire(started + ACTION_MESSAGE_DURATION - Duration::from_millis(1));
        assert_eq!(message.text(), "focused project-one:1.0");

        message.expire(started + ACTION_MESSAGE_DURATION);
        assert!(message.text().is_empty());
    }

    #[test]
    fn newer_transient_message_restarts_the_three_second_window() {
        let started = Instant::now();
        let mut message = UiMessage::default();
        message.set_transient("focused project-one:1.0", started);
        message.set_transient("refreshed", started + Duration::from_secs(2));

        message.expire(started + ACTION_MESSAGE_DURATION);
        assert_eq!(message.text(), "refreshed");

        message.expire(started + Duration::from_secs(5));
        assert!(message.text().is_empty());
    }

    #[test]
    fn daemon_errors_do_not_expire_on_the_action_timer() {
        let started = Instant::now();
        let mut message = UiMessage::default();
        message.set_daemon_error("daemon: socket unavailable");

        message.expire(started + ACTION_MESSAGE_DURATION);

        assert_eq!(message.text(), "daemon: socket unavailable");
    }

    #[test]
    fn footer_restores_key_hints_after_transient_message_expires() {
        let started = Instant::now();
        let mut message = UiMessage::default();
        message.set_transient("focused project-one:1.0", started);
        let snapshot = Snapshot::default();
        let area = Rect::new(0, 0, 80, 10);
        let footer_row = ui_layout(area, false)[3].y;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, message.text(), 0))
            .unwrap();
        assert_eq!(row_text(&terminal, footer_row), "focused project-one:1.0");

        message.expire(started + ACTION_MESSAGE_DURATION);
        terminal
            .draw(|frame| render(frame, &snapshot, 0, message.text(), 0))
            .unwrap();
        assert_eq!(
            row_text(&terminal, footer_row),
            "j/k move  enter focus/view  r refresh  q close"
        );
    }

    #[test]
    fn peer_dots_are_green_when_connected_and_red_when_disconnected() {
        let snapshot = Snapshot {
            peers: vec![
                crate::model::PeerStatus {
                    name: "build-host".into(),
                    connected: true,
                    last_error: None,
                    application_version: Some("0.1.0".into()),
                    protocol: crate::model::PROTOCOL_VERSION,
                    capabilities: Vec::new(),
                },
                crate::model::PeerStatus {
                    name: "offline-host".into(),
                    connected: false,
                    last_error: Some("connection refused".into()),
                    application_version: None,
                    protocol: 0,
                    capabilities: Vec::new(),
                },
            ],
            ..Snapshot::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 0))
            .unwrap();

        let dots = (0..100)
            .filter(|x| terminal.backend().buffer()[(*x, 7)].symbol() == "●")
            .collect::<Vec<_>>();
        assert_eq!(dots.len(), 2);
        assert_eq!(terminal.backend().buffer()[(dots[0], 7)].fg, Color::Green);
        assert_eq!(terminal.backend().buffer()[(dots[1], 7)].fg, Color::Red);
        assert!(row_text(&terminal, 7).contains("build-host v0.1.0"));
        assert!(row_text(&terminal, 7).contains("offline-host (connection refused)"));
    }

    #[test]
    fn redraws_first_frame_but_not_content_only_updates() {
        let mut snapshot = Snapshot {
            agents: vec![test_agent("Codex", Attention::Working, AgentOrigin::Tmux)],
            ..Snapshot::default()
        };
        let mut redraw = RedrawTracker::default();
        let topology = RenderTopology::from_snapshot(&snapshot);

        assert!(redraw.needs_full_redraw(&topology));
        redraw.mark_rendered(topology);

        snapshot.agents[0].attention = Attention::Done;
        snapshot.agents[0].title = "finished-task".into();
        assert!(!redraw.needs_full_redraw(&RenderTopology::from_snapshot(&snapshot)));
    }

    #[test]
    fn redraws_when_rows_are_inserted_removed_or_reordered() {
        let codex = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        let claude = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        let original = Snapshot {
            agents: vec![codex.clone(), claude.clone()],
            ..Snapshot::default()
        };
        let mut redraw = RedrawTracker::default();
        redraw.mark_rendered(RenderTopology::from_snapshot(&original));

        let inserted = Snapshot {
            agents: vec![
                codex.clone(),
                claude.clone(),
                test_agent("OpenCode", Attention::Idle, AgentOrigin::Tmux),
            ],
            ..Snapshot::default()
        };
        assert!(redraw.needs_full_redraw(&RenderTopology::from_snapshot(&inserted)));

        let removed = Snapshot {
            agents: vec![codex.clone()],
            ..Snapshot::default()
        };
        assert!(redraw.needs_full_redraw(&RenderTopology::from_snapshot(&removed)));

        let reordered = Snapshot {
            agents: vec![claude, codex],
            ..Snapshot::default()
        };
        assert!(redraw.needs_full_redraw(&RenderTopology::from_snapshot(&reordered)));
    }

    #[test]
    fn redraws_when_row_height_or_peer_section_changes() {
        let parent = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        let mut redraw = RedrawTracker::default();
        let original = Snapshot {
            agents: vec![parent.clone()],
            ..Snapshot::default()
        };
        redraw.mark_rendered(RenderTopology::from_snapshot(&original));

        let mut child = parent.clone();
        child.subagent = Some(SubagentInfo {
            parent_id: "local/default/parent".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let compact_row = Snapshot {
            agents: vec![child],
            ..Snapshot::default()
        };
        assert!(redraw.needs_full_redraw(&RenderTopology::from_snapshot(&compact_row)));

        let with_peer = Snapshot {
            agents: vec![parent],
            peers: vec![crate::model::PeerStatus {
                name: "remote-mac".into(),
                connected: true,
                last_error: None,
                application_version: Some("0.1.0".into()),
                protocol: crate::model::PROTOCOL_VERSION,
                capabilities: Vec::new(),
            }],
            ..Snapshot::default()
        };
        assert!(redraw.needs_full_redraw(&RenderTopology::from_snapshot(&with_peer)));
    }

    #[test]
    fn forced_redraw_is_consumed_after_a_successful_render() {
        let snapshot = Snapshot {
            agents: vec![test_agent("Codex", Attention::Working, AgentOrigin::Tmux)],
            ..Snapshot::default()
        };
        let topology = RenderTopology::from_snapshot(&snapshot);
        let mut redraw = RedrawTracker::default();
        redraw.mark_rendered(topology.clone());
        assert!(!redraw.needs_full_redraw(&topology));

        redraw.force();
        assert!(redraw.needs_full_redraw(&topology));
        redraw.mark_rendered(topology.clone());
        assert!(!redraw.needs_full_redraw(&topology));
    }

    #[test]
    fn truncates_by_character_count() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }

    #[test]
    fn working_glyph_animates_while_other_states_remain_static() {
        assert_eq!(spinner_frame(Duration::ZERO), 0);
        assert_eq!(spinner_frame(SPINNER_FRAME_TIME), 1);
        assert_eq!(
            spinner_frame(SPINNER_FRAME_TIME * SPINNER_FRAMES.len() as u32),
            0
        );
        assert_eq!(attention_glyph(Attention::Working, 0), "⠋");
        assert_eq!(attention_glyph(Attention::Working, 5), "⠴");
        assert_eq!(attention_glyph(Attention::Blocked, 5), "!");
        assert_eq!(attention_glyph(Attention::Done, 5), "✓");
    }

    #[test]
    fn goal_durations_follow_codex_compact_units() {
        assert_eq!(format_goal_duration(42), "42s");
        assert_eq!(format_goal_duration(1_122), "18m 42s");
        assert_eq!(format_goal_duration(7_920), "2h 12m");
        assert_eq!(format_goal_duration(93_784), "1d 2h");
    }

    #[test]
    fn goal_labels_have_full_and_compact_forms() {
        let goal = GoalInfo {
            state: GoalState::Pursuing,
            elapsed_seconds: 1_122,
            achievement_pending: false,
            achievement_observed_at_ms: 0,
        };
        assert_eq!(goal_label(&goal, false), "Pursuing goal (18m 42s)");
        assert_eq!(goal_label(&goal, true), "goal (18m 42s)");
    }

    #[test]
    fn activation_only_acknowledges_a_pending_goal_achievement() {
        let mut agent = test_agent("Codex", Attention::Idle, AgentOrigin::Tmux);
        agent.goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        });
        assert!(has_pending_goal_achievement(&agent));

        agent.state = AgentState::Working;
        assert!(!has_pending_goal_achievement(&agent));

        agent.state = AgentState::Idle;
        agent.goal.as_mut().unwrap().achievement_pending = false;
        assert!(!has_pending_goal_achievement(&agent));

        agent.goal = Some(GoalInfo {
            state: GoalState::Pursuing,
            elapsed_seconds: 5,
            achievement_pending: false,
            achievement_observed_at_ms: 0,
        });
        assert!(!has_pending_goal_achievement(&agent));
    }

    #[test]
    fn activating_a_done_agent_requires_acknowledgement() {
        let mut agent = test_agent("Codex", Attention::Done, AgentOrigin::Tmux);
        agent.state = AgentState::Idle;
        assert!(activation_requires_acknowledgement(&agent));

        agent.attention = Attention::Idle;
        assert!(!activation_requires_acknowledgement(&agent));
    }

    #[test]
    fn provider_badges_are_fixed_width_and_visually_distinct() {
        let (codex, codex_style) = provider_badge("Codex");
        let (claude, claude_style) = provider_badge("Claude");
        let (opencode, opencode_style) = provider_badge("OpenCode");
        let (pi, pi_style) = provider_badge("Pi");

        assert_eq!(codex, "CODEX   ");
        assert_eq!(claude, "CLAUDE  ");
        assert_eq!(opencode, "OPENCODE");
        assert_eq!(pi, "PI      ");
        assert_ne!(codex_style.bg, claude_style.bg);
        assert_ne!(claude_style.bg, opencode_style.bg);
        assert_ne!(opencode_style.bg, pi_style.bg);
    }

    #[test]
    fn working_spinners_match_their_provider_badge_foregrounds() {
        let snapshot = Snapshot {
            agents: vec![
                test_agent("Codex", Attention::Working, AgentOrigin::Tmux),
                test_agent("OpenCode", Attention::Working, AgentOrigin::Tmux),
                test_agent("Claude", Attention::Working, AgentOrigin::Tmux),
            ],
            ..Snapshot::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();

        for row in [4, 6, 8] {
            let spinner = &terminal.backend().buffer()[(1, row)];
            let provider_badge = &terminal.backend().buffer()[(3, row)];
            assert_eq!(spinner.fg, provider_badge.fg);
        }
    }

    #[test]
    fn locations_render_as_host_first_breadcrumbs() {
        let tmux_agent = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        assert_eq!(
            location_breadcrumb(&tmux_agent),
            "remote-mac › project-one › 1.0"
        );

        let terminal_agent = test_agent("Claude", Attention::Blocked, AgentOrigin::Terminal);
        assert_eq!(
            location_breadcrumb(&terminal_agent),
            "remote-mac › tty ttys005"
        );
    }

    #[test]
    fn explicitly_mapped_remote_tmux_breadcrumb_stays_remote() {
        let mut remote = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        remote.remote_alias = Some("remote-mac".into());
        remote.session_name = "wmtc-manual-48".into();
        remote.window_index = 0;
        remote.pane_index = 0;
        remote.label = Some("testing env".into());

        assert!(remote.focus_target.is_none());
        assert_eq!(
            location_breadcrumb(&remote),
            "remote-mac › wmtc-manual-48 › 0.0"
        );
        assert_eq!(remote.location_label(), "wmtc-manual-48:0.0");
    }

    #[test]
    fn resolved_remote_terminal_uses_local_tmux_breadcrumb() {
        let mut terminal_agent = test_agent("Codex", Attention::Unknown, AgentOrigin::Terminal);
        terminal_agent.remote_alias = Some("remote-mac".into());
        terminal_agent.focus_target = Some(crate::model::TmuxTarget {
            session_name: "workspace".into(),
            window_id: "@57".into(),
            window_index: 1,
            pane_id: "%57".into(),
            pane_index: 0,
        });
        assert_eq!(
            location_breadcrumb(&terminal_agent),
            "remote-mac › workspace › 1.0"
        );
        assert_eq!(terminal_agent.location_label(), "workspace:1.0");
    }

    #[test]
    fn display_title_removes_the_provider_spinner() {
        let mut agent = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        agent.title = "⠦ sample-project".into();
        assert_eq!(display_title(&agent), "sample-project");

        agent.title = "⠹".into();
        assert_eq!(display_title(&agent), "work");
    }

    #[test]
    fn display_title_keeps_grok_working_directory_stable() {
        let mut agent = test_agent("Grok", Attention::Working, AgentOrigin::Tmux);
        agent.cwd = "/work/sample-project".into();
        agent.title = "⠦ Analyzing changes - grok".into();
        assert_eq!(display_title(&agent), "sample-project");

        agent.title = "⠸ Running cargo test - grok".into();
        assert_eq!(display_title(&agent), "sample-project");

        agent.cwd = "/".into();
        assert_eq!(display_title(&agent), "/");
    }

    #[test]
    fn display_title_appends_the_pane_label() {
        let mut agent = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        agent.title = "⠦ sample-project".into();
        agent.label = Some("  remote development  ".into());
        assert_eq!(display_title(&agent), "sample-project | remote development");

        agent.label = Some("   ".into());
        assert_eq!(display_title(&agent), "sample-project");

        agent.label = Some("sample-project".into());
        assert_eq!(display_title(&agent), "sample-project");
    }

    #[test]
    fn blocked_and_selected_rows_use_distinct_backgrounds() {
        assert_eq!(
            agent_row_style(Attention::Blocked, false).bg,
            Some(Color::Rgb(45, 20, 24))
        );
        assert_eq!(
            agent_row_style(Attention::Blocked, true).bg,
            Some(Color::Rgb(65, 25, 30))
        );
        assert_eq!(
            agent_row_style(Attention::Working, true).bg,
            Some(Color::Rgb(35, 45, 55))
        );
        assert_eq!(agent_row_style(Attention::Idle, false).bg, None);
    }

    #[test]
    fn rendered_rows_show_spinner_badge_title_breadcrumb_and_blocked_tint() {
        let mut working = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        working.title = "⠦ long-project-name".into();
        let snapshot = Snapshot {
            agents: vec![
                working,
                test_agent("Claude", Attention::Blocked, AgentOrigin::Tmux),
            ],
            ..Snapshot::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();

        assert!(row_text(&terminal, 4).contains("▌⠴ CODEX"));
        assert!(row_text(&terminal, 4).contains("long-project-name"));
        assert!(!row_text(&terminal, 4).contains("⠦"));
        assert!(row_text(&terminal, 5).contains("remote-mac › project-one › 1.0"));
        assert_eq!(
            terminal.backend().buffer()[(69, 4)].bg,
            Color::Rgb(35, 45, 55)
        );
        assert_eq!(
            terminal.backend().buffer()[(69, 6)].bg,
            Color::Rgb(45, 20, 24)
        );
    }

    #[test]
    fn rendered_goal_follows_activity_in_codex_magenta() {
        let mut working = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        working.state = AgentState::Working;
        working.title = "sample-project".into();
        working.goal = Some(GoalInfo {
            state: GoalState::Pursuing,
            elapsed_seconds: 1_122,
            achievement_pending: false,
            achievement_observed_at_ms: 0,
        });
        let snapshot = Snapshot {
            agents: vec![working],
            ..Snapshot::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();

        let row = row_text(&terminal, 4);
        assert!(row.contains("working  Pursuing goal (18m 42s)"));
        let goal_x = row.find("Pursuing goal").unwrap() as u16;
        assert_eq!(terminal.backend().buffer()[(goal_x, 4)].fg, Color::Magenta);
    }

    #[test]
    fn rendered_achievement_disappears_only_after_acknowledgement() {
        let mut achieved = test_agent("Codex", Attention::Idle, AgentOrigin::Tmux);
        achieved.state = AgentState::Idle;
        achieved.title = "overnight-task".into();
        achieved.goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        });
        let mut snapshot = Snapshot {
            agents: vec![achieved],
            ..Snapshot::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();

        snapshot.agents[0].state = AgentState::Working;
        snapshot.agents[0].attention = Attention::Working;
        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();
        assert!(!row_text(&terminal, 4).contains("Goal achieved"));

        snapshot.agents[0].state = AgentState::Idle;
        snapshot.agents[0].attention = Attention::Idle;
        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();
        assert!(row_text(&terminal, 4).contains("Goal achieved (2h 12m)"));

        snapshot.agents[0]
            .goal
            .as_mut()
            .unwrap()
            .achievement_pending = false;
        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();
        assert!(!row_text(&terminal, 4).contains("Goal achieved"));
    }

    #[test]
    fn subagents_render_as_subtle_indented_single_lines() {
        let mut parent = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        parent.id = "local/default/parent".into();
        parent.title = "sample-project".into();
        let mut child = test_agent("Codex", Attention::Unknown, AgentOrigin::Terminal);
        child.id = "local/terminal/ttys054/70".into();
        child.title = "sample-project".into();
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 1_000,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut snapshot = Snapshot {
            generated_at_ms: 121_000,
            agents: vec![child, parent],
            ..Snapshot::default()
        };
        snapshot.sort_agents();
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();

        assert!(row_text(&terminal, 4).contains("sample-project"));
        assert!(row_text(&terminal, 4).contains("+1 agent"));
        assert!(row_text(&terminal, 6).contains("↳ subagent: review"));
        assert!(row_text(&terminal, 6).contains("running  ·  2m 0s"));
        assert!(!row_text(&terminal, 6).contains("CODEX"));
        assert!(!row_text(&terminal, 6).contains("remote-mac ›"));
        let title_x = (0..90)
            .find(|x| terminal.backend().buffer()[(*x, 4)].symbol() == "s")
            .unwrap();
        let arrow_x = (0..90)
            .find(|x| terminal.backend().buffer()[(*x, 6)].symbol() == "↳")
            .unwrap();
        assert_eq!(arrow_x, title_x);
        assert_eq!(terminal.backend().buffer()[(arrow_x, 6)].fg, Color::Yellow);
        assert_eq!(terminal.backend().buffer()[(89, 6)].bg, Color::Reset);
    }

    #[test]
    fn process_child_follows_and_indents_below_its_thread_parent() {
        let mut root = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        root.id = "local/default/root".into();
        root.title = "sample-project".into();
        let mut worker = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        worker.id = "local/codex-thread/worker".into();
        worker.subagent = Some(SubagentInfo {
            parent_id: root.id.clone(),
            started_at_ms: 1_000,
            finished_at_ms: None,
            name: Some("Worker".into()),
            thread_id: Some("worker".into()),
        });
        let mut review = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        review.id = "local/terminal/review".into();
        review.subagent = Some(SubagentInfo {
            parent_id: worker.id.clone(),
            started_at_ms: 2_000,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut snapshot = Snapshot {
            generated_at_ms: 121_000,
            agents: vec![review, worker, root],
            ..Snapshot::default()
        };
        snapshot.sort_agents();
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();

        assert!(row_text(&terminal, 6).contains("subagent: Worker"));
        assert!(row_text(&terminal, 7).contains("subagent: review"));
        let worker_arrow = (0..90)
            .find(|x| terminal.backend().buffer()[(*x, 6)].symbol() == "↳")
            .unwrap();
        let review_arrow = (0..90)
            .find(|x| terminal.backend().buffer()[(*x, 7)].symbol() == "↳")
            .unwrap();
        assert_eq!(review_arrow, worker_arrow + 2);
    }

    #[test]
    fn completed_subagent_uses_green_done_state_and_frozen_duration() {
        let mut parent = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        parent.id = "local/default/parent".into();
        let mut child = test_agent("Codex", Attention::Done, AgentOrigin::Terminal);
        child.id = "local/terminal/ttys054/70".into();
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 1_000,
            finished_at_ms: Some(253_000),
            name: Some("review".into()),
            thread_id: None,
        });
        let mut snapshot = Snapshot {
            generated_at_ms: 270_000,
            agents: vec![parent, child],
            ..Snapshot::default()
        };
        snapshot.sort_agents();
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();

        assert!(!row_text(&terminal, 4).contains("+1 agent"));
        assert!(row_text(&terminal, 6).contains("done  ·  4m 12s"));
        let done_x = row_text(&terminal, 6).find("done").unwrap() as u16;
        assert_eq!(terminal.backend().buffer()[(done_x, 6)].fg, Color::Green);
        assert_eq!(terminal.backend().buffer()[(89, 6)].bg, Color::Reset);
    }

    #[test]
    fn remote_subagent_view_requires_an_advertised_capability() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/opt/tmux-agent/bin/tmux-agent"
            "#,
        )
        .unwrap();
        let mut child = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        child.id = "remote/remote-mac/host/terminal/ttys001/70".into();
        child.remote_alias = Some("remote-mac".into());
        child.subagent = Some(SubagentInfo {
            parent_id: "remote/remote-mac/host/default/%1".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let snapshot = Snapshot {
            peers: vec![crate::model::PeerStatus {
                name: "remote-mac".into(),
                connected: true,
                last_error: None,
                application_version: Some("0.0.9".into()),
                protocol: crate::model::PROTOCOL_VERSION,
                capabilities: Vec::new(),
            }],
            ..Snapshot::default()
        };

        let error = subagent_view_command(&config, Path::new("/tmp/config"), &snapshot, &child)
            .unwrap_err();
        assert!(error.to_string().contains("remote-mac"));
        assert!(error.to_string().contains("Update"));
    }

    #[test]
    fn local_subagent_view_uses_the_exact_record_and_config() {
        let config = Config::default();
        let mut child = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        child.id = "local/terminal/ttys001/70".into();
        child.subagent = Some(SubagentInfo {
            parent_id: "local/default/%1".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });

        let command = subagent_view_command(
            &config,
            Path::new("/tmp/tmux-agent-config.toml"),
            &Snapshot::default(),
            &child,
        )
        .unwrap();

        assert_eq!(
            command,
            [
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                "--config".into(),
                "/tmp/tmux-agent-config.toml".into(),
                "subagent-view".into(),
                "--local-only".into(),
                child.id,
            ]
        );
    }

    #[test]
    fn compatible_remote_subagent_view_uses_structured_ssh() {
        let config: Config = toml::from_str(
            r#"
                [[machine]]
                name = "remote-mac"
                host = "remote-mac.example.ts.net"
                ssh_user = "agent"
                binary = "/opt/tmux-agent/bin/tmux-agent"
            "#,
        )
        .unwrap();
        let mut child = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        child.id = "remote/remote-mac/host/terminal/ttys001/70".into();
        child.remote_alias = Some("remote-mac".into());
        child.subagent = Some(SubagentInfo {
            parent_id: "remote/remote-mac/host/default/%1".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let snapshot = Snapshot {
            peers: vec![crate::model::PeerStatus {
                name: "remote-mac".into(),
                connected: true,
                last_error: None,
                application_version: Some("0.1.0".into()),
                protocol: crate::model::PROTOCOL_VERSION,
                capabilities: vec![crate::model::CAPABILITY_SUBAGENT_VIEW.into()],
            }],
            ..Snapshot::default()
        };

        let command =
            subagent_view_command(&config, Path::new("/tmp/config"), &snapshot, &child).unwrap();
        assert_eq!(command.first().map(String::as_str), Some("ssh"));
        assert!(command.last().unwrap().contains("subagent-view"));
    }

    #[tokio::test]
    async fn unavailable_remote_focus_acknowledges_pending_goal_on_activation() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!("tmux-agent-ui-focus-{}-{nonce}", std::process::id());
        let config = Config {
            tmux_args: vec!["-L".into(), socket_name.clone()],
            ..Config::default()
        };
        let paths = RuntimePaths::discover(&socket_name).unwrap();
        paths.ensure_dirs().unwrap();
        let listener = UnixListener::bind(&paths.socket).unwrap();
        let mut remote = test_agent("Codex", Attention::Idle, AgentOrigin::Tmux);
        remote.id = "remote/remote-mac/host/default/%1".into();
        remote.remote_alias = Some("remote-mac".into());
        remote.title = "completed-task".into();
        remote.goal = Some(GoalInfo {
            state: GoalState::Achieved,
            elapsed_seconds: 7_920,
            achievement_pending: true,
            achievement_observed_at_ms: 123_000,
        });
        let mut acknowledged_snapshot = Snapshot {
            agents: vec![remote.clone()],
            ..Snapshot::default()
        };
        acknowledged_snapshot.agents[0]
            .goal
            .as_mut()
            .unwrap()
            .achievement_pending = false;
        let server = tokio::spawn(async move {
            let mut acknowledged = None;
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                match serde_json::from_str::<crate::model::IpcRequest>(&line).unwrap() {
                    crate::model::IpcRequest::Acknowledge { target } => {
                        acknowledged = Some(target);
                        let mut response =
                            serde_json::to_vec(&crate::model::IpcResponse::Ack).unwrap();
                        response.push(b'\n');
                        writer.write_all(&response).await.unwrap();
                    }
                    crate::model::IpcRequest::Snapshot { .. } => {
                        let mut response =
                            serde_json::to_vec(&crate::model::IpcResponse::Snapshot {
                                snapshot: acknowledged_snapshot.clone(),
                            })
                            .unwrap();
                        response.push(b'\n');
                        writer.write_all(&response).await.unwrap();
                    }
                    request => panic!("unexpected request: {request:?}"),
                }
            }
            acknowledged
        });
        let started = Command::new("tmux")
            .args([
                "-L",
                &socket_name,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "ui-focus-test",
            ])
            .status()
            .unwrap();
        assert!(started.success());
        let tmux = Tmux::new(&config);
        let mut snapshot = Snapshot {
            agents: vec![remote],
            ..Snapshot::default()
        };
        let context = ActivationContext {
            paths: &paths,
            tmux: &tmux,
            config: &config,
            config_path: Path::new("/tmp/tmux-agent-config.toml"),
            exit_after_focus: false,
        };
        let mut message = UiMessage::default();

        let activation = activate_record(&context, &mut snapshot, 0, &mut message).await;

        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        let acknowledged = server.await.unwrap();
        let _ = std::fs::remove_file(&paths.socket);
        let _ = std::fs::remove_dir(&paths.runners);
        assert_eq!(activation.unwrap(), Activation::Continue);
        assert_eq!(
            acknowledged.as_deref(),
            Some("remote/remote-mac/host/default/%1")
        );
        assert!(message.text().contains("acknowledged"));
        assert!(
            !snapshot.agents[0]
                .goal
                .as_ref()
                .unwrap()
                .achievement_pending
        );
    }

    #[tokio::test]
    async fn popup_ui_replaces_itself_with_the_read_only_child_view() {
        let config = Config::default();
        let paths = RuntimePaths::discover("ui-popup-test").unwrap();
        let tmux = Tmux::new(&config);
        let mut child = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        child.id = "local/terminal/ttys001/70".into();
        child.subagent = Some(SubagentInfo {
            parent_id: "local/default/%1".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut snapshot = Snapshot {
            agents: vec![child],
            ..Snapshot::default()
        };
        let context = ActivationContext {
            paths: &paths,
            tmux: &tmux,
            config: &config,
            config_path: Path::new("/tmp/tmux-agent-config.toml"),
            exit_after_focus: true,
        };
        let mut message = UiMessage::default();

        let activation = activate_record(&context, &mut snapshot, 0, &mut message)
            .await
            .unwrap();

        let Activation::RunInCurrentTerminal(command) = activation else {
            panic!("popup UI should hand its terminal to the child viewer");
        };
        assert!(command.iter().any(|argument| argument == "subagent-view"));
        assert_eq!(command.last(), Some(&snapshot.agents[0].id));
        assert!(message.text().is_empty());
    }

    #[test]
    fn mouse_rows_select_two_line_agent_items() {
        let area = Rect::new(0, 0, 80, 20);
        let agents = (0..8)
            .map(|_| test_agent("Codex", Attention::Idle, AgentOrigin::Tmux))
            .collect::<Vec<_>>();
        assert_eq!(agent_at_mouse(area, true, 0, &agents, 4, 4), Some(0));
        assert_eq!(agent_at_mouse(area, true, 0, &agents, 4, 5), Some(0));
        assert_eq!(agent_at_mouse(area, true, 0, &agents, 4, 6), Some(1));
        assert_eq!(agent_at_mouse(area, true, 0, &agents, 4, 3), None);
    }

    #[test]
    fn mouse_rows_follow_single_line_subagent_items() {
        let area = Rect::new(0, 0, 80, 20);
        let parent = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        let mut child = test_agent("Codex", Attention::Unknown, AgentOrigin::Terminal);
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: None,
            thread_id: None,
        });
        let agents = vec![
            parent,
            child,
            test_agent("Claude", Attention::Idle, AgentOrigin::Tmux),
        ];
        assert_eq!(agent_at_mouse(area, true, 0, &agents, 4, 5), Some(0));
        assert_eq!(agent_at_mouse(area, true, 0, &agents, 4, 6), Some(1));
        assert_eq!(agent_at_mouse(area, true, 0, &agents, 4, 7), Some(2));
    }

    #[test]
    fn mouse_rows_follow_the_selected_list_scroll() {
        let area = Rect::new(0, 0, 80, 20);
        let agents = (0..10)
            .map(|_| test_agent("Codex", Attention::Idle, AgentOrigin::Tmux))
            .collect::<Vec<_>>();
        assert_eq!(agent_at_mouse(area, true, 9, &agents, 4, 4), Some(4));
    }

    #[test]
    fn mouse_ignores_unrendered_remainder_row() {
        let area = Rect::new(0, 0, 80, 11);
        let agents = (0..4)
            .map(|_| test_agent("Codex", Attention::Idle, AgentOrigin::Tmux))
            .collect::<Vec<_>>();
        assert_eq!(agent_at_mouse(area, false, 0, &agents, 4, 8), None);
    }
}
