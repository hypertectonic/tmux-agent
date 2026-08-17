use crate::config::{Config, RuntimePaths, shell_join};
use crate::ipc;
use crate::model::{
    AgentOrigin, AgentRecord, AgentState, Attention, GoalInfo, GoalState, Snapshot, terminal_safe,
    trim_braille_activity_prefix,
};
use crate::tmux::{Tmux, is_focus_target_missing};
use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};
use std::collections::{HashMap, HashSet};
use std::future::pending;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAME_TIME: Duration = Duration::from_millis(120);
const ELAPSED_TIME_TICK: Duration = Duration::from_secs(1);
const VISIBLE_VISIBILITY_PROBE_INTERVAL: Duration = Duration::from_millis(250);
const HIDDEN_VISIBILITY_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const WATCH_RECONNECT_INTERVAL: Duration = Duration::from_millis(500);
const ACTION_MESSAGE_DURATION: Duration = Duration::from_secs(3);
const PROVIDER_WIDTH: usize = 8;
const PROVIDER_TITLE_GAP: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiTimer {
    AnimationFrame,
    ElapsedTime,
    VisibilityProbe,
}

#[derive(Debug)]
struct UiSchedule {
    visible: bool,
    dirty: bool,
    rendered_once: bool,
    render_while_hidden_once: bool,
    next_animation_frame: Instant,
    next_elapsed_time: Instant,
    next_visibility_probe: Option<Instant>,
}

impl UiSchedule {
    fn new(visible: bool, now: Instant) -> Self {
        Self {
            visible,
            dirty: true,
            rendered_once: false,
            render_while_hidden_once: false,
            next_animation_frame: now + SPINNER_FRAME_TIME,
            next_elapsed_time: now + ELAPSED_TIME_TICK,
            next_visibility_probe: Some(now + visibility_probe_interval(visible)),
        }
    }

    fn always_visible(now: Instant) -> Self {
        Self {
            visible: true,
            dirty: true,
            rendered_once: false,
            render_while_hidden_once: false,
            next_animation_frame: now + SPINNER_FRAME_TIME,
            next_elapsed_time: now + ELAPSED_TIME_TICK,
            next_visibility_probe: None,
        }
    }

    fn should_render(&self) -> bool {
        self.dirty && (self.visible || !self.rendered_once || self.render_while_hidden_once)
    }

    fn rendered(&mut self) {
        self.dirty = false;
        self.rendered_once = true;
        self.render_while_hidden_once = false;
    }

    fn view_changed(&mut self) {
        self.dirty = true;
    }

    fn shared_selection_changed(&mut self) {
        self.dirty = true;
        self.render_while_hidden_once = true;
    }

    fn next_timer(
        &self,
        has_working_agents: bool,
        has_running_subagents: bool,
    ) -> Option<(Instant, UiTimer)> {
        let animation = (self.visible && has_working_agents)
            .then_some((self.next_animation_frame, UiTimer::AnimationFrame));
        let elapsed_time = (self.visible && has_running_subagents)
            .then_some((self.next_elapsed_time, UiTimer::ElapsedTime));
        let visibility = self
            .next_visibility_probe
            .map(|deadline| (deadline, UiTimer::VisibilityProbe));
        let mut next = animation;
        for candidate in [elapsed_time, visibility].into_iter().flatten() {
            if next.is_none_or(|current| candidate.0 < current.0) {
                next = Some(candidate);
            }
        }
        next
    }

    fn timer_elapsed(&mut self, timer: UiTimer, now: Instant) {
        match timer {
            UiTimer::AnimationFrame => {
                self.dirty = true;
                self.next_animation_frame = now + SPINNER_FRAME_TIME;
            }
            UiTimer::ElapsedTime => {
                self.dirty = true;
                self.next_elapsed_time = now + ELAPSED_TIME_TICK;
            }
            UiTimer::VisibilityProbe => {}
        }
    }

    fn visibility_checked(&mut self, visible: bool, now: Instant) {
        if visible && !self.visible {
            self.dirty = true;
            self.next_animation_frame = now + SPINNER_FRAME_TIME;
            self.next_elapsed_time = now + ELAPSED_TIME_TICK;
        }
        self.visible = visible;
        self.next_visibility_probe = Some(now + visibility_probe_interval(visible));
    }
}

fn visibility_probe_interval(visible: bool) -> Duration {
    if visible {
        VISIBLE_VISIBILITY_PROBE_INTERVAL
    } else {
        HIDDEN_VISIBILITY_PROBE_INTERVAL
    }
}

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
        pane_id: pane_id.clone(),
    };
    let mut terminal = ratatui::init();
    if let Err(error) = execute!(io::stdout(), EnableMouseCapture) {
        ratatui::restore();
        return Err(error).context("enable mouse capture");
    }
    let result = run_loop(
        &mut terminal,
        paths,
        &tmux,
        popup,
        pane_id.as_deref(),
        config,
        config_path,
    )
    .await;
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
    #[cfg(test)]
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        let visible_indices = (0..snapshot.agents.len()).collect::<Vec<_>>();
        Self::from_visible(snapshot, &visible_indices)
    }

    fn from_visible(snapshot: &Snapshot, visible_indices: &[usize]) -> Self {
        Self {
            has_peers: !snapshot.peers.is_empty(),
            rows: visible_indices
                .iter()
                .map(|index| {
                    let agent = &snapshot.agents[*index];
                    (agent.id.clone(), agent_row_height(agent))
                })
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

    fn set_daemon_error(&mut self, text: impl Into<String>) -> bool {
        let text = text.into();
        if matches!(self, Self::DaemonError(current) if current == &text) {
            return false;
        }
        *self = Self::DaemonError(text);
        true
    }

    fn clear_daemon_error(&mut self) -> bool {
        if matches!(self, Self::DaemonError(_)) {
            *self = Self::None;
            return true;
        }
        false
    }

    fn expire(&mut self, now: Instant) -> bool {
        if matches!(self, Self::Transient { expires_at, .. } if now >= *expires_at) {
            *self = Self::None;
            return true;
        }
        false
    }

    fn expires_at(&self) -> Option<Instant> {
        match self {
            Self::Transient { expires_at, .. } => Some(*expires_at),
            Self::None | Self::DaemonError(_) => None,
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::None => "",
            Self::Transient { text, .. } | Self::DaemonError(text) => text,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AgentListState {
    searching: bool,
    query: String,
    selected_id: Option<String>,
    selected_position: usize,
}

impl AgentListState {
    fn enter_search(&mut self) {
        self.searching = true;
    }

    fn push_query(&mut self, character: char) {
        self.query.push(character);
    }

    fn visible_indices(&self, agents: &[AgentRecord]) -> Vec<usize> {
        let query = self.query.to_lowercase();
        agents
            .iter()
            .enumerate()
            .filter_map(|(index, agent)| agent_matches_query(agent, &query).then_some(index))
            .collect()
    }

    fn reconcile_selection(&mut self, agents: &[AgentRecord]) {
        let visible = self.visible_indices(agents);
        if visible.is_empty() {
            return;
        }
        if let Some(position) = self.selected_id.as_deref().and_then(|selected_id| {
            visible
                .iter()
                .position(|index| agents[*index].id == selected_id)
        }) {
            self.selected_position = position;
            return;
        }
        self.selected_position = self.selected_position.min(visible.len() - 1);
        self.selected_id = Some(agents[visible[self.selected_position]].id.clone());
    }

    fn selected_visible_index(&self, agents: &[AgentRecord]) -> Option<usize> {
        let selected_id = self.selected_id.as_deref()?;
        self.visible_indices(agents)
            .iter()
            .position(|index| agents[*index].id == selected_id)
    }

    fn select_visible(&mut self, agents: &[AgentRecord], visible_index: usize) {
        let visible = self.visible_indices(agents);
        if let Some(index) = visible.get(visible_index) {
            self.selected_id = Some(agents[*index].id.clone());
            self.selected_position = visible_index;
        }
    }

    fn select_snapshot(&mut self, agents: &[AgentRecord], snapshot_index: usize) {
        if let Some(agent) = agents.get(snapshot_index) {
            self.selected_id = Some(agent.id.clone());
            if let Some(position) = self
                .visible_indices(agents)
                .iter()
                .position(|index| *index == snapshot_index)
            {
                self.selected_position = position;
            }
        }
    }

    fn move_selection(&mut self, agents: &[AgentRecord], direction: isize) {
        let visible = self.visible_indices(agents);
        if visible.is_empty() {
            return;
        }
        let current = self.selected_visible_index(agents).unwrap_or_default();
        let next = if direction < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(visible.len() - 1)
        };
        self.selected_id = Some(agents[visible[next]].id.clone());
        self.selected_position = next;
    }

    fn selected_snapshot_index(&self, agents: &[AgentRecord]) -> Option<usize> {
        let selected_id = self.selected_id.as_deref()?;
        self.visible_indices(agents)
            .into_iter()
            .find(|index| agents[*index].id == selected_id)
    }

    fn leave_search(&mut self) {
        self.searching = false;
        self.query.clear();
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RenderedSessionShortcuts {
    agent_ids: Vec<String>,
}

impl RenderedSessionShortcuts {
    fn from_list(list: &AgentListState, agents: &[AgentRecord]) -> Self {
        if list.searching {
            return Self::default();
        }
        Self {
            agent_ids: list
                .visible_indices(agents)
                .into_iter()
                .filter(|index| agents[*index].subagent.is_none())
                .take(10)
                .map(|index| agents[index].id.clone())
                .collect(),
        }
    }

    fn snapshot_index(&self, agents: &[AgentRecord], slot: usize) -> Option<usize> {
        let agent_id = self.agent_ids.get(slot)?;
        agents.iter().position(|agent| &agent.id == agent_id)
    }

    fn key_for(&self, agent_id: &str) -> Option<char> {
        let slot = self.agent_ids.iter().position(|id| id == agent_id)?;
        match slot {
            0..=8 => char::from_digit(slot as u32 + 1, 10),
            9 => Some('0'),
            _ => None,
        }
    }
}

fn agent_matches_query(agent: &AgentRecord, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let title = display_title(agent);
    let location = location_breadcrumb(agent);
    [
        agent.agent.as_str(),
        title.as_str(),
        agent.label.as_deref().unwrap_or_default(),
        agent.host.as_str(),
        agent.session_name.as_str(),
        agent.window_name.as_str(),
        agent.cwd.as_str(),
        location.as_str(),
        attention_label(agent.attention),
    ]
    .into_iter()
    .any(|value| value.to_lowercase().contains(query))
        || agent.subagent.as_ref().is_some_and(|subagent| {
            subagent
                .name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(query))
                || subagent_state_label(subagent.finished_at_ms).contains(query)
        })
}

fn subagent_state_label(finished_at_ms: Option<u64>) -> &'static str {
    if finished_at_ms.is_some() {
        "done"
    } else {
        "running"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListAction {
    None,
    Close,
    Activate,
    ActivateShortcut(usize),
    SyncSharedSelection,
    Refresh,
}

const UI_SELECTION_WAKE_KEY: u8 = 17;

fn shortcut_slot(character: char) -> Option<usize> {
    match character {
        '1'..='9' => Some(character as usize - '1' as usize),
        '0' => Some(9),
        _ => None,
    }
}

fn handle_list_key(key: KeyEvent, list: &mut AgentListState, agents: &[AgentRecord]) -> ListAction {
    if key.code == KeyCode::F(UI_SELECTION_WAKE_KEY) {
        return ListAction::SyncSharedSelection;
    }
    let action = if list.searching {
        match key.code {
            KeyCode::Esc => list.leave_search(),
            KeyCode::Enter => return ListAction::Activate,
            KeyCode::Down => list.move_selection(agents, 1),
            KeyCode::Up => list.move_selection(agents, -1),
            KeyCode::Backspace => {
                list.query.pop();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                list.push_query(character);
            }
            _ => {}
        }
        ListAction::None
    } else {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return ListAction::Close,
            KeyCode::Char('j') | KeyCode::Down => list.move_selection(agents, 1),
            KeyCode::Char('k') | KeyCode::Up => list.move_selection(agents, -1),
            KeyCode::Char('g') | KeyCode::Home => list.select_visible(agents, 0),
            KeyCode::Char('G') | KeyCode::End => {
                list.select_visible(agents, list.visible_indices(agents).len().saturating_sub(1));
            }
            KeyCode::Enter => return ListAction::Activate,
            KeyCode::Char('r') => return ListAction::Refresh,
            KeyCode::Char('/') => list.enter_search(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(slot) = shortcut_slot(character) {
                    return ListAction::ActivateShortcut(slot);
                }
            }
            _ => {}
        }
        ListAction::None
    };
    list.reconcile_selection(agents);
    action
}

fn apply_shared_selection(
    list: &mut AgentListState,
    agents: &[AgentRecord],
    agent_id: &str,
) -> bool {
    let Some(index) = agents.iter().position(|agent| agent.id == agent_id) else {
        return false;
    };
    let previous = list.clone();
    list.leave_search();
    list.select_snapshot(agents, index);
    *list != previous
}

fn apply_pending_shared_selection(
    pending: &mut Option<String>,
    list: &mut AgentListState,
    agents: &[AgentRecord],
) -> bool {
    let Some(agent_id) = pending.as_deref() else {
        return false;
    };
    if !agents.iter().any(|agent| agent.id == agent_id) {
        return false;
    }
    let changed = apply_shared_selection(list, agents, agent_id);
    pending.take();
    changed
}

fn broadcast_ui_selection_in_background(tmux: &Tmux, agent_id: String) {
    let tmux = tmux.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = tmux.broadcast_ui_selection(&agent_id) {
            eprintln!("tmux-agent: synchronize UI selection: {error:#}");
        }
    });
}

async fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    paths: &RuntimePaths,
    tmux: &Tmux,
    exit_after_focus: bool,
    ui_pane_id: Option<&str>,
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
    let (initial_watch, mut snapshot) = connect_snapshot_watch(&paths.socket).await?;
    let mut watch = Some(initial_watch);
    let mut list = AgentListState::default();
    list.reconcile_selection(&snapshot.agents);
    let mut message = UiMessage::default();
    let started = Instant::now();
    let mut schedule = match ui_pane_id {
        Some(pane_id) => UiSchedule::new(read_pane_visibility(tmux, pane_id).await?, started),
        None => UiSchedule::always_visible(started),
    };
    let animation_started = started;
    let mut redraw = RedrawTracker::default();
    let mut rendered_shortcuts = RenderedSessionShortcuts::default();
    let mut pending_shared_selection = None;
    let mut terminal_events = EventStream::new();
    let mut reconnect_at = None;

    loop {
        if apply_pending_shared_selection(
            &mut pending_shared_selection,
            &mut list,
            &snapshot.agents,
        ) {
            schedule.shared_selection_changed();
        }
        if schedule.should_render() {
            list.reconcile_selection(&snapshot.agents);
            let visible_indices = list.visible_indices(&snapshot.agents);
            let next_shortcuts = RenderedSessionShortcuts::from_list(&list, &snapshot.agents);
            let topology = RenderTopology::from_visible(&snapshot, &visible_indices);
            if redraw.needs_full_redraw(&topology) {
                let area = terminal
                    .size()
                    .context("read terminal size for full redraw")?
                    .into();
                terminal
                    .resize(area)
                    .context("prepare terminal for full redraw")?;
            }
            terminal.draw(|frame| {
                render_agent_list(
                    frame,
                    &snapshot,
                    &list,
                    message.text(),
                    spinner_frame(animation_started.elapsed()),
                    current_time_ms(),
                )
            })?;
            rendered_shortcuts = next_shortcuts;
            redraw.mark_rendered(topology);
            schedule.rendered();
        }

        let visible_indices = list.visible_indices(&snapshot.agents);
        let (has_working_agents, has_running_subagents) =
            visible_timer_requirements(&snapshot.agents, &visible_indices);
        let timer = next_loop_timer(
            &schedule,
            has_working_agents,
            has_running_subagents,
            message.expires_at(),
            reconnect_at,
        );
        tokio::select! {
            terminal_event = terminal_events.next() => match terminal_event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    let previous_list = list.clone();
                    match handle_list_key(key, &mut list, &snapshot.agents) {
                        ListAction::None => {
                            if list != previous_list {
                                schedule.view_changed();
                            }
                        }
                        ListAction::Close => return Ok(LoopExit::Close),
                        action @ (ListAction::Activate | ListAction::ActivateShortcut(_)) => {
                            let selected = match action {
                                ListAction::Activate => {
                                    list.selected_snapshot_index(&snapshot.agents)
                                }
                                ListAction::ActivateShortcut(slot) => {
                                    let selected = rendered_shortcuts
                                        .snapshot_index(&snapshot.agents, slot);
                                    if let Some(index) = selected {
                                        list.select_snapshot(&snapshot.agents, index);
                                    }
                                    selected
                                }
                                _ => unreachable!(),
                            };
                            let Some(selected) = selected else { continue };
                            let shared_selection = matches!(action, ListAction::ActivateShortcut(_))
                                .then(|| snapshot.agents.get(selected).map(|agent| agent.id.clone()))
                                .flatten();
                            let activation = activate_record(
                                &activation_context,
                                &mut snapshot,
                                selected,
                                &mut message,
                            )
                            .await?;
                            if let Some(agent_id) = shared_selection {
                                broadcast_ui_selection_in_background(tmux, agent_id);
                            }
                            if let Some(exit) =
                                apply_activation_outcome(activation, &mut list, &snapshot.agents)
                            {
                                return Ok(exit);
                            }
                            schedule.view_changed();
                        }
                        ListAction::SyncSharedSelection => {
                            let agent_id = match tmux.ui_selection() {
                                Ok(Some(agent_id)) => agent_id,
                                Ok(None) => continue,
                                Err(error) => {
                                    eprintln!(
                                        "tmux-agent: read synchronized UI selection: {error:#}"
                                    );
                                    continue;
                                }
                            };
                            pending_shared_selection = Some(agent_id);
                        }
                        ListAction::Refresh => {
                            let (next_watch, next_snapshot) =
                                connect_snapshot_watch(&paths.socket).await?;
                            watch = Some(next_watch);
                            snapshot = next_snapshot;
                            list.reconcile_selection(&snapshot.agents);
                            reconnect_at = None;
                            message.set_transient("refreshed", Instant::now());
                            redraw.force();
                            schedule.view_changed();
                        }
                    }
                }
                Some(Ok(Event::Mouse(mouse)))
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
                {
                    let (width, height) =
                        crossterm::terminal::size().context("read terminal size")?;
                    let visible_indices = list.visible_indices(&snapshot.agents);
                    let selected = list
                        .selected_visible_index(&snapshot.agents)
                        .unwrap_or_default();
                    if let Some(index) = agent_at_mouse_filtered(
                        Rect::new(0, 0, width, height),
                        !snapshot.peers.is_empty(),
                        selected,
                        &snapshot.agents,
                        &visible_indices,
                        mouse.column,
                        mouse.row,
                    ) {
                        list.select_snapshot(&snapshot.agents, index);
                        let activation = activate_record(
                            &activation_context,
                            &mut snapshot,
                            index,
                            &mut message,
                        )
                        .await?;
                        if let Some(exit) =
                            apply_activation_outcome(activation, &mut list, &snapshot.agents)
                        {
                            return Ok(exit);
                        }
                        schedule.view_changed();
                    }
                }
                Some(Ok(Event::Resize(_, _))) => {
                    schedule.view_changed();
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error).context("read terminal input"),
                None => return Ok(LoopExit::Close),
            },
            watched_snapshot = next_watched_snapshot(&mut watch) => {
                match watched_snapshot {
                    Ok(Some(next)) => {
                        apply_daemon_refresh(&mut snapshot, &mut message, Ok(next));
                        list.reconcile_selection(&snapshot.agents);
                        schedule.view_changed();
                    }
                    Ok(None) => {
                        if message.set_daemon_error("daemon: watch stream closed") {
                            schedule.view_changed();
                        }
                        watch = None;
                        reconnect_at = Some(Instant::now() + WATCH_RECONNECT_INTERVAL);
                    }
                    Err(error) => {
                        if message.set_daemon_error(format!("daemon: {error:#}")) {
                            schedule.view_changed();
                        }
                        watch = None;
                        reconnect_at = Some(Instant::now() + WATCH_RECONNECT_INTERVAL);
                    }
                }
            },
            timer = wait_for_timer(timer) => {
                let now = Instant::now();
                match timer {
                    LoopTimer::Ui(UiTimer::AnimationFrame) => {
                        schedule.timer_elapsed(UiTimer::AnimationFrame, now);
                    }
                    LoopTimer::Ui(UiTimer::ElapsedTime) => {
                        schedule.timer_elapsed(UiTimer::ElapsedTime, now);
                    }
                    LoopTimer::Ui(UiTimer::VisibilityProbe) => {
                        let pane_id = ui_pane_id.context("visibility timer without a UI pane")?;
                        let visible = read_pane_visibility(tmux, pane_id).await?;
                        schedule.visibility_checked(visible, Instant::now());
                    }
                    LoopTimer::MessageExpiry => {
                        if message.expire(now) {
                            schedule.view_changed();
                        }
                    }
                    LoopTimer::WatchReconnect => {
                        match connect_snapshot_watch(&paths.socket).await {
                            Ok((next_watch, next_snapshot)) => {
                                watch = Some(next_watch);
                                reconnect_at = None;
                                apply_daemon_refresh(
                                    &mut snapshot,
                                    &mut message,
                                    Ok(next_snapshot),
                                );
                                list.reconcile_selection(&snapshot.agents);
                                schedule.view_changed();
                            }
                            Err(error) => {
                                if message.set_daemon_error(format!("daemon: {error:#}")) {
                                    schedule.view_changed();
                                }
                                reconnect_at =
                                    Some(Instant::now() + WATCH_RECONNECT_INTERVAL);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn visible_timer_requirements(agents: &[AgentRecord], visible_indices: &[usize]) -> (bool, bool) {
    visible_indices.iter().fold(
        (false, false),
        |(has_working_agent, has_running_subagent), index| {
            let agent = &agents[*index];
            match &agent.subagent {
                Some(subagent) => (
                    has_working_agent,
                    has_running_subagent || subagent.finished_at_ms.is_none(),
                ),
                None => (
                    has_working_agent || agent.attention == Attention::Working,
                    has_running_subagent,
                ),
            }
        },
    )
}

#[derive(Clone, Copy, Debug)]
enum LoopTimer {
    Ui(UiTimer),
    MessageExpiry,
    WatchReconnect,
}

fn next_loop_timer(
    schedule: &UiSchedule,
    has_working_agents: bool,
    has_running_subagents: bool,
    message_expiry: Option<Instant>,
    reconnect_at: Option<Instant>,
) -> Option<(Instant, LoopTimer)> {
    let mut next = schedule
        .next_timer(has_working_agents, has_running_subagents)
        .map(|(deadline, timer)| (deadline, LoopTimer::Ui(timer)));
    for candidate in [
        message_expiry.map(|deadline| (deadline, LoopTimer::MessageExpiry)),
        reconnect_at.map(|deadline| (deadline, LoopTimer::WatchReconnect)),
    ]
    .into_iter()
    .flatten()
    {
        if next.is_none_or(|current| candidate.0 < current.0) {
            next = Some(candidate);
        }
    }
    next
}

async fn wait_for_timer(timer: Option<(Instant, LoopTimer)>) -> LoopTimer {
    match timer {
        Some((deadline, timer)) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            timer
        }
        None => pending().await,
    }
}

async fn next_watched_snapshot(watch: &mut Option<ipc::SnapshotWatch>) -> Result<Option<Snapshot>> {
    match watch {
        Some(watch) => watch.next_snapshot().await,
        None => pending().await,
    }
}

async fn connect_snapshot_watch(socket: &Path) -> Result<(ipc::SnapshotWatch, Snapshot)> {
    let mut watch = ipc::SnapshotWatch::connect(socket, false).await?;
    let snapshot = watch
        .next_snapshot()
        .await?
        .context("daemon closed watch stream before its initial snapshot")?;
    Ok((watch, snapshot))
}

async fn read_pane_visibility(tmux: &Tmux, pane_id: &str) -> Result<bool> {
    let tmux = tmux.clone();
    let pane_id = pane_id.to_string();
    tokio::task::spawn_blocking(move || tmux.pane_visible(&pane_id))
        .await
        .context("join tmux pane visibility probe")?
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
        Err(error) => {
            message.set_daemon_error(format!("daemon: {error:#}"));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Activation {
    Completed,
    Failed,
    Close,
    RunInCurrentTerminal(Vec<String>),
}

fn apply_activation_outcome(
    activation: Activation,
    list: &mut AgentListState,
    agents: &[AgentRecord],
) -> Option<LoopExit> {
    match activation {
        Activation::Completed => {
            list.leave_search();
            list.reconcile_selection(agents);
            None
        }
        Activation::Failed => None,
        Activation::Close => Some(LoopExit::Close),
        Activation::RunInCurrentTerminal(command) => Some(LoopExit::RunInCurrentTerminal(command)),
    }
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
        return Ok(Activation::Failed);
    };
    if record.subagent.is_some() {
        let command =
            match subagent_view_command(context.config, context.config_path, snapshot, &record) {
                Ok(command) => command,
                Err(error) if !context.exit_after_focus => {
                    message.set_transient(format!("{error:#}"), Instant::now());
                    return Ok(Activation::Failed);
                }
                Err(error) => return Err(error),
            };
        if context.exit_after_focus {
            return Ok(Activation::RunInCurrentTerminal(command));
        }
        return match context.tmux.display_popup(&shell_join(&command)) {
            Ok(()) => {
                message.set_transient("opened read-only subagent view", Instant::now());
                Ok(Activation::Completed)
            }
            Err(error) => {
                message.set_transient(format!("{error:#}"), Instant::now());
                Ok(Activation::Failed)
            }
        };
    }
    let focus_record = &record;
    if focus_record.is_tmux() || focus_record.remote_alias.is_some() {
        return match context.tmux.focus_agent(focus_record) {
            Ok(()) => {
                // Usage ordering is optional metadata. A successful focus must
                // remain successful when an older daemon cannot record it.
                let _ = ipc::mark_used(&context.paths.socket, &record.id).await;
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
                Ok(Activation::Completed)
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
                Ok(Activation::Completed)
            }
            Err(error) => {
                message.set_transient(format!("{error:#}"), Instant::now());
                Ok(Activation::Failed)
            }
        };
    }
    match acknowledge_record(context.paths, snapshot, &record.id).await {
        Ok(()) => {
            message.set_transient(
                format!("acknowledged {}", record.location()),
                Instant::now(),
            );
            Ok(Activation::Completed)
        }
        Err(error) => {
            message.set_transient(format!("{error:#}"), Instant::now());
            Ok(Activation::Failed)
        }
    }
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

#[cfg(test)]
fn render(
    frame: &mut Frame,
    snapshot: &Snapshot,
    selected: usize,
    message: &str,
    spinner_frame: usize,
) {
    render_at(
        frame,
        snapshot,
        selected,
        message,
        spinner_frame,
        snapshot.generated_at_ms,
    );
}

#[cfg(test)]
fn render_live(
    frame: &mut Frame,
    snapshot: &Snapshot,
    selected: usize,
    message: &str,
    spinner_frame: usize,
) {
    render_at(
        frame,
        snapshot,
        selected,
        message,
        spinner_frame,
        current_time_ms(),
    );
}

#[cfg(test)]
fn render_at(
    frame: &mut Frame,
    snapshot: &Snapshot,
    selected: usize,
    message: &str,
    spinner_frame: usize,
    rendered_at_ms: u64,
) {
    let list = AgentListState {
        selected_id: snapshot.agents.get(selected).map(|agent| agent.id.clone()),
        ..AgentListState::default()
    };
    render_agent_list(
        frame,
        snapshot,
        &list,
        message,
        spinner_frame,
        rendered_at_ms,
    );
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn render_agent_list(
    frame: &mut Frame,
    snapshot: &Snapshot,
    list: &AgentListState,
    message: &str,
    spinner_frame: usize,
    rendered_at_ms: u64,
) {
    let chunks = ui_layout(frame.area(), !snapshot.peers.is_empty());
    let visible_indices = list.visible_indices(&snapshot.agents);
    let shortcuts = RenderedSessionShortcuts::from_list(list, &snapshot.agents);
    let selected = list
        .selected_visible_index(&snapshot.agents)
        .unwrap_or_default();

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
    let search_line = if list.searching {
        Line::from(vec![
            Span::styled(" search ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                terminal_safe(&list.query),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("█", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::default()
    };
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
        search_line,
    ])
    .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, chunks[0]);

    let list_width = chunks[1].width.saturating_sub(1) as usize;
    let items = visible_indices
        .iter()
        .enumerate()
        .map(|(visible_index, snapshot_index)| {
            let agent = &snapshot.agents[*snapshot_index];
            if let Some(subagent) = &agent.subagent {
                let finished = subagent.finished_at_ms.is_some();
                let state_color = if finished { Color::Green } else { Color::Cyan };
                let state_label = subagent_state_label(subagent.finished_at_ms);
                let end = subagent
                    .finished_at_ms
                    .unwrap_or(rendered_at_ms.max(subagent.started_at_ms));
                let duration = format_duration(
                    end.saturating_sub(subagent.started_at_ms)
                        .saturating_div(1_000),
                );
                let name = subagent.name.as_deref().unwrap_or("agent");
                let depth = subagent_depths.get(&agent.id).copied().unwrap_or(1);
                return ListItem::new(Line::from(vec![
                    Span::raw(" ".repeat(
                        2 + PROVIDER_WIDTH + PROVIDER_TITLE_GAP + depth.saturating_sub(1) * 2,
                    )),
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
                    2 + PROVIDER_WIDTH
                        + PROVIDER_TITLE_GAP
                        + state_width
                        + goal_width
                        + child_count_width,
                )
                .max(1);
            let location_width = list_width
                .saturating_sub(2 + PROVIDER_WIDTH + PROVIDER_TITLE_GAP)
                .max(1);
            let (provider, provider_style) = provider_badge(&agent.agent);
            let shortcut = shortcuts.key_for(&agent.id);
            let is_selected = visible_index == selected;
            let row_style = agent_row_style(agent.attention, is_selected);
            let shortcut_style = if is_selected {
                provider_style
            } else {
                Style::default()
                    .fg(Color::Rgb(145, 165, 171))
                    .bg(Color::Rgb(45, 45, 45))
            };
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
                Span::raw(" "),
                shortcut.map_or_else(
                    || Span::raw(" "),
                    |key| Span::styled(key.to_string(), shortcut_style),
                ),
                Span::raw(" "),
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
                    Span::raw(" ".repeat(2 + PROVIDER_WIDTH + PROVIDER_TITLE_GAP)),
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
    if visible_indices.is_empty() && list.searching && !list.query.is_empty() {
        frame.render_widget(
            Paragraph::new(" no sessions match").style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
    }
    let agent_list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(Line::from(Span::styled(
            "▌",
            Style::default().fg(Color::Cyan),
        )))
        .highlight_spacing(HighlightSpacing::Always)
        .repeat_highlight_symbol(true);
    let mut list_state = ListState::default();
    if !visible_indices.is_empty() {
        list_state.select(Some(selected));
    }
    frame.render_stateful_widget(agent_list, chunks[1], &mut list_state);

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
    let footer = if !message.is_empty() {
        terminal_safe(message)
    } else if list.searching {
        "↑/↓ move  enter focus/view  backspace edit  esc clear".to_string()
    } else {
        "j/k move  / search  enter focus/view  r refresh  q close".to_string()
    };
    frame.render_widget(
        Paragraph::new(truncate(
            &footer,
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

#[cfg(test)]
fn agent_at_mouse(
    area: Rect,
    has_peers: bool,
    selected: usize,
    agents: &[AgentRecord],
    column: u16,
    row: u16,
) -> Option<usize> {
    let visible_indices = (0..agents.len()).collect::<Vec<_>>();
    agent_at_mouse_filtered(
        area,
        has_peers,
        selected,
        agents,
        &visible_indices,
        column,
        row,
    )
}

fn agent_at_mouse_filtered(
    area: Rect,
    has_peers: bool,
    selected: usize,
    agents: &[AgentRecord],
    visible_indices: &[usize],
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
    let heights = visible_indices
        .iter()
        .map(|index| agent_row_height(&agents[*index]))
        .collect::<Vec<_>>();
    let offset = visible_list_offset(&heights, selected, usize::from(list.height));
    let mut relative_row = usize::from(row - list.y);
    let mut rendered_height = 0usize;
    for (index, height) in heights.into_iter().enumerate().skip(offset) {
        if rendered_height.saturating_add(height) > usize::from(list.height) {
            break;
        }
        if relative_row < height {
            return visible_indices.get(index).copied();
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
        "omp" => ("OMP", Color::Rgb(230, 160, 190), Color::Rgb(65, 35, 50)),
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
            "j/k move  / search  enter focus/view  r refresh  q close"
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
    fn ui_schedule_draws_initially_and_after_changes_not_unchanged_probes() {
        let started = Instant::now();
        let mut schedule = UiSchedule::new(true, started);

        assert!(schedule.should_render());
        schedule.rendered();
        assert!(!schedule.should_render());

        assert_eq!(
            schedule.next_timer(true, false),
            Some((started + SPINNER_FRAME_TIME, UiTimer::AnimationFrame))
        );
        schedule.timer_elapsed(UiTimer::AnimationFrame, started + SPINNER_FRAME_TIME);
        assert!(schedule.should_render());
        schedule.rendered();

        schedule.view_changed();
        assert!(schedule.should_render());
        schedule.rendered();

        let (deadline, timer) = schedule.next_timer(false, false).unwrap();
        assert_eq!(deadline, started + VISIBLE_VISIBILITY_PROBE_INTERVAL);
        assert_eq!(timer, UiTimer::VisibilityProbe);
        schedule.visibility_checked(true, deadline);
        assert!(!schedule.should_render());
    }

    #[test]
    fn visible_timer_requirements_follow_rendered_rows() {
        let working_root = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        let idle_root = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        let mut running_subagent = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        running_subagent.subagent = Some(SubagentInfo {
            parent_id: working_root.id.clone(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let agents = vec![working_root, idle_root, running_subagent];

        assert_eq!(visible_timer_requirements(&agents, &[1]), (false, false));
        assert_eq!(visible_timer_requirements(&agents, &[2]), (false, true));
        assert_eq!(visible_timer_requirements(&agents, &[0]), (true, false));
        assert_eq!(visible_timer_requirements(&agents, &[0, 2]), (true, true));
    }

    #[test]
    fn hidden_schedule_defers_updates_and_animation() {
        let started = Instant::now();
        let mut schedule = UiSchedule::new(false, started);
        assert!(schedule.should_render());
        schedule.rendered();
        schedule.view_changed();

        assert!(!schedule.should_render());
        assert_eq!(
            schedule.next_timer(true, true),
            Some((
                started + HIDDEN_VISIBILITY_PROBE_INTERVAL,
                UiTimer::VisibilityProbe
            ))
        );

        let hidden_probe = started + HIDDEN_VISIBILITY_PROBE_INTERVAL;
        schedule.visibility_checked(false, hidden_probe);
        assert!(!schedule.should_render());

        schedule.visibility_checked(true, hidden_probe);
        assert!(schedule.should_render());
    }

    #[test]
    fn explicit_shared_selection_renders_once_while_hidden() {
        let started = Instant::now();
        let mut schedule = UiSchedule::new(false, started);
        schedule.rendered();

        schedule.shared_selection_changed();
        assert!(schedule.should_render());

        schedule.rendered();
        assert!(!schedule.should_render());
        assert!(!schedule.visible);
    }

    #[test]
    fn running_subagent_duration_advances_without_a_new_snapshot() {
        let now_ms = crate::scanner::now_ms();
        let mut parent = test_agent("Codex", Attention::Idle, AgentOrigin::Tmux);
        parent.id = "local/default/parent".into();
        let mut child = test_agent("Codex", Attention::Unknown, AgentOrigin::Terminal);
        child.id = "local/terminal/ttys054/70".into();
        child.subagent = Some(SubagentInfo {
            parent_id: parent.id.clone(),
            started_at_ms: now_ms - 2_000,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut snapshot = Snapshot {
            generated_at_ms: now_ms,
            agents: vec![parent, child],
            ..Snapshot::default()
        };
        snapshot.sort_agents();
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();

        terminal
            .draw(|frame| render_live(frame, &snapshot, 0, "", 0))
            .unwrap();
        assert!(row_text(&terminal, 6).contains("running  ·  2s"));

        std::thread::sleep(Duration::from_millis(1_100));
        terminal
            .draw(|frame| render_live(frame, &snapshot, 0, "", 0))
            .unwrap();

        assert!(row_text(&terminal, 6).contains("running  ·  3s"));

        let started = Instant::now();
        let mut schedule = UiSchedule::always_visible(started);
        schedule.rendered();
        assert_eq!(
            schedule.next_timer(false, true),
            Some((started + Duration::from_secs(1), UiTimer::ElapsedTime))
        );
        schedule.timer_elapsed(UiTimer::ElapsedTime, started + Duration::from_secs(1));
        assert!(schedule.should_render());
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
        let (omp, omp_style) = provider_badge("OMP");
        let (pi, pi_style) = provider_badge("Pi");

        assert_eq!(codex, "CODEX   ");
        assert_eq!(claude, "CLAUDE  ");
        assert_eq!(opencode, "OPENCODE");
        assert_eq!(omp, "OMP     ");
        assert_eq!(omp_style.fg, Some(Color::Rgb(230, 160, 190)));
        assert_eq!(omp_style.bg, Some(Color::Rgb(65, 35, 50)));
        assert_eq!(pi, "PI      ");
        assert_ne!(codex_style.bg, claude_style.bg);
        assert_ne!(claude_style.bg, opencode_style.bg);
        assert_ne!(opencode_style.bg, omp_style.bg);
        assert_ne!(omp_style.bg, pi_style.bg);
        assert_ne!(opencode_style.bg, pi_style.bg);
    }

    #[test]
    fn working_spinners_match_their_provider_badge_foregrounds() {
        let snapshot = Snapshot {
            agents: vec![
                test_agent("Codex", Attention::Working, AgentOrigin::Tmux),
                test_agent("OpenCode", Attention::Working, AgentOrigin::Tmux),
                test_agent("Claude", Attention::Working, AgentOrigin::Tmux),
                test_agent("OMP", Attention::Working, AgentOrigin::Tmux),
            ],
            ..Snapshot::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 5))
            .unwrap();

        for row in [4, 6, 8, 10] {
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
    fn search_filters_agents_by_visible_text_case_insensitively() {
        let mut codex = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        codex.title = "Payment API".into();
        let mut claude = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        claude.title = "Documentation".into();
        let agents = vec![codex, claude];
        let mut list = AgentListState::default();

        list.enter_search();
        for character in "PAYMENT".chars() {
            list.push_query(character);
        }

        assert_eq!(list.visible_indices(&agents), vec![0]);
    }

    #[test]
    fn search_matches_provider_label_location_and_working_directory() {
        let mut codex = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        codex.id = "local/default/payment".into();
        codex.title = "Payment API".into();
        codex.label = Some("priority work".into());
        codex.host = "build-host".into();
        codex.session_name = "backend".into();
        codex.cwd = "/work/services/payments".into();
        let mut claude = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        claude.id = "local/default/docs".into();
        claude.title = "Documentation".into();
        let agents = vec![codex, claude];

        for query in ["codex", "priority", "build-host", "backend", "payments"] {
            let mut list = AgentListState::default();
            list.enter_search();
            for character in query.chars() {
                list.push_query(character);
            }
            assert_eq!(list.visible_indices(&agents), vec![0], "query {query:?}");
        }
    }

    #[test]
    fn search_matches_the_state_displayed_for_subagents() {
        let mut running = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        running.subagent = Some(SubagentInfo {
            parent_id: "local/default/parent".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut done = running.clone();
        done.id = "local/default/done".into();
        done.subagent.as_mut().unwrap().finished_at_ms = Some(2);
        let agents = vec![running, done];

        let mut list = AgentListState::default();
        list.enter_search();
        for character in "running".chars() {
            list.push_query(character);
        }
        assert_eq!(list.visible_indices(&agents), vec![0]);

        list.query = "done".into();
        assert_eq!(list.visible_indices(&agents), vec![1]);
    }

    #[test]
    fn search_mode_treats_navigation_commands_as_query_text() {
        let agents = vec![test_agent("Codex", Attention::Working, AgentOrigin::Tmux)];
        let mut list = AgentListState::default();
        list.reconcile_selection(&agents);

        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::Close
        );
        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::None
        );
        assert!(list.searching);

        for character in ['j', 'k', 'r', 'q'] {
            assert_eq!(
                handle_list_key(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                    &mut list,
                    &agents,
                ),
                ListAction::None
            );
        }
        assert_eq!(list.query, "jkrq");

        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::None
        );
        assert!(!list.searching);
        assert!(list.query.is_empty());
    }

    #[test]
    fn number_keys_request_top_level_shortcuts_only_outside_search() {
        let agents = vec![test_agent("Codex", Attention::Working, AgentOrigin::Tmux)];
        let mut list = AgentListState::default();
        list.reconcile_selection(&agents);

        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::ActivateShortcut(0)
        );
        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::ActivateShortcut(9)
        );

        list.enter_search();
        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::None
        );
        assert_eq!(list.query, "1");
    }

    #[test]
    fn shared_selection_wake_is_not_treated_as_user_input() {
        let agents = vec![test_agent("Codex", Attention::Working, AgentOrigin::Tmux)];
        let mut list = AgentListState::default();
        list.reconcile_selection(&agents);

        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::F(17), KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::SyncSharedSelection
        );
    }

    #[test]
    fn shared_numeric_selection_updates_other_sidebar_instances() {
        let mut first = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        first.id = "local/default/first".into();
        let mut second = test_agent("Codex", Attention::Idle, AgentOrigin::Tmux);
        second.id = "local/default/second".into();
        let agents = vec![first, second];
        let mut source_sidebar = AgentListState::default();
        let mut destination_sidebar = AgentListState::default();
        source_sidebar.reconcile_selection(&agents);
        destination_sidebar.reconcile_selection(&agents);

        assert!(apply_shared_selection(
            &mut source_sidebar,
            &agents,
            "local/default/second"
        ));
        assert!(apply_shared_selection(
            &mut destination_sidebar,
            &agents,
            "local/default/second"
        ));
        assert_eq!(
            source_sidebar.selected_id.as_deref(),
            Some("local/default/second")
        );
        assert_eq!(
            destination_sidebar.selected_id.as_deref(),
            Some("local/default/second")
        );
    }

    #[test]
    fn shared_numeric_selection_waits_for_the_destination_snapshot() {
        let mut first = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        first.id = "local/default/first".into();
        let mut second = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        second.id = "local/default/second".into();
        let mut list = AgentListState::default();
        list.reconcile_selection(std::slice::from_ref(&first));
        list.enter_search();
        list.push_query('f');
        let mut pending = Some(second.id.clone());

        assert!(!apply_pending_shared_selection(
            &mut pending,
            &mut list,
            std::slice::from_ref(&first)
        ));
        assert_eq!(pending.as_deref(), Some("local/default/second"));
        assert!(list.searching);
        assert_eq!(list.query, "f");

        assert!(apply_pending_shared_selection(
            &mut pending,
            &mut list,
            &[first, second]
        ));
        assert_eq!(pending, None);
        assert!(!list.searching);
        assert!(list.query.is_empty());
        assert_eq!(list.selected_id.as_deref(), Some("local/default/second"));
    }

    #[test]
    fn rendered_shortcuts_keep_top_level_targets_stable_across_snapshot_changes() {
        let mut first = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        first.id = "local/default/first".into();
        let mut child = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        child.id = "local/terminal/child".into();
        child.subagent = Some(SubagentInfo {
            parent_id: first.id.clone(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        let mut second = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        second.id = "local/default/second".into();
        let rendered = vec![first.clone(), child, second.clone()];
        let list = AgentListState::default();

        let shortcuts = RenderedSessionShortcuts::from_list(&list, &rendered);
        let current = vec![second, first];

        assert_eq!(shortcuts.snapshot_index(&current, 0), Some(1));
        assert_eq!(shortcuts.snapshot_index(&current, 1), Some(0));
        assert_eq!(shortcuts.snapshot_index(&current, 2), None);
    }

    #[test]
    fn search_renders_only_matching_rows_and_an_active_prompt() {
        let mut payment = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        payment.id = "local/default/payment".into();
        payment.title = "Payment API".into();
        let mut docs = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        docs.id = "local/default/docs".into();
        docs.title = "Documentation".into();
        let snapshot = Snapshot {
            agents: vec![payment, docs],
            ..Snapshot::default()
        };
        let mut list = AgentListState::default();
        list.enter_search();
        for character in "doc".chars() {
            list.push_query(character);
        }
        list.reconcile_selection(&snapshot.agents);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();

        terminal
            .draw(|frame| {
                render_agent_list(frame, &snapshot, &list, "", 0, snapshot.generated_at_ms)
            })
            .unwrap();

        let screen = (0..12)
            .map(|row| row_text(&terminal, row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Documentation"));
        assert!(!screen.contains("Payment API"));
        assert!(screen.contains("search doc█"));
    }

    #[test]
    fn search_keeps_operational_messages_visible() {
        let snapshot = Snapshot::default();
        let mut list = AgentListState::default();
        list.enter_search();
        for character in "doc".chars() {
            list.push_query(character);
        }
        let area = Rect::new(0, 0, 80, 10);
        let search_row = 2;
        let footer_row = ui_layout(area, false)[3].y;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();

        terminal
            .draw(|frame| {
                render_agent_list(
                    frame,
                    &snapshot,
                    &list,
                    "focus failed",
                    0,
                    snapshot.generated_at_ms,
                )
            })
            .unwrap();

        let footer = row_text(&terminal, footer_row);
        assert!(footer.contains("focus failed"));
        assert!(row_text(&terminal, search_row).contains("search doc█"));
    }

    #[test]
    fn filtered_selection_and_mouse_activation_use_snapshot_indices() {
        let mut payment = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        payment.id = "local/default/payment".into();
        payment.title = "Payment API".into();
        let mut docs = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        docs.id = "local/default/docs".into();
        docs.title = "Documentation".into();
        let agents = vec![payment, docs];
        let mut list = AgentListState::default();
        list.enter_search();
        for character in "doc".chars() {
            list.push_query(character);
        }
        list.reconcile_selection(&agents);
        let visible = list.visible_indices(&agents);

        assert_eq!(visible, vec![1]);
        assert_eq!(list.selected_snapshot_index(&agents), Some(1));
        assert_eq!(
            handle_list_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut list,
                &agents,
            ),
            ListAction::Activate
        );
        assert_eq!(
            agent_at_mouse_filtered(Rect::new(0, 0, 80, 12), false, 0, &agents, &visible, 4, 4),
            Some(1)
        );
    }

    #[test]
    fn successful_activation_clears_search_and_keeps_the_activated_record_selected() {
        let mut payment = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        payment.id = "local/default/payment".into();
        payment.title = "Payment API".into();
        let mut docs = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        docs.id = "local/default/docs".into();
        docs.title = "Documentation".into();
        let agents = vec![payment, docs];
        let mut list = AgentListState::default();
        list.enter_search();
        for character in "doc".chars() {
            list.push_query(character);
        }
        list.reconcile_selection(&agents);

        let exit = apply_activation_outcome(Activation::Completed, &mut list, &agents);

        assert_eq!(exit, None);
        assert!(!list.searching);
        assert!(list.query.is_empty());
        assert_eq!(list.visible_indices(&agents), vec![0, 1]);
        assert_eq!(list.selected_snapshot_index(&agents), Some(1));
        assert_eq!(list.selected_id.as_deref(), Some("local/default/docs"));
    }

    #[test]
    fn failed_activation_keeps_the_active_search() {
        let mut payment = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        payment.id = "local/default/payment".into();
        payment.title = "Payment API".into();
        let mut docs = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        docs.id = "local/default/docs".into();
        docs.title = "Documentation".into();
        let agents = vec![payment, docs];
        let mut list = AgentListState::default();
        list.enter_search();
        for character in "doc".chars() {
            list.push_query(character);
        }
        list.reconcile_selection(&agents);

        let exit = apply_activation_outcome(Activation::Failed, &mut list, &agents);

        assert_eq!(exit, None);
        assert!(list.searching);
        assert_eq!(list.query, "doc");
        assert_eq!(list.visible_indices(&agents), vec![1]);
        assert_eq!(list.selected_snapshot_index(&agents), Some(1));
    }

    #[test]
    fn search_keeps_selection_by_agent_id_across_snapshot_refreshes() {
        let mut payment = test_agent("Codex", Attention::Working, AgentOrigin::Tmux);
        payment.id = "local/default/payment".into();
        payment.title = "Payment API".into();
        let mut docs = test_agent("Claude", Attention::Idle, AgentOrigin::Tmux);
        docs.id = "local/default/docs".into();
        docs.title = "Documentation".into();
        let mut agents = vec![payment, docs];
        let mut list = AgentListState::default();
        list.reconcile_selection(&agents);
        list.move_selection(&agents, 1);
        assert_eq!(list.selected_snapshot_index(&agents), Some(1));

        agents.swap(0, 1);
        list.reconcile_selection(&agents);

        assert_eq!(list.selected_snapshot_index(&agents), Some(0));
        assert_eq!(list.selected_id.as_deref(), Some("local/default/docs"));
    }

    #[test]
    fn selection_keeps_its_position_when_the_selected_agent_disappears() {
        let mut agents = ["one", "two", "three", "four"]
            .into_iter()
            .map(|id| {
                let mut agent = test_agent("Codex", Attention::Idle, AgentOrigin::Tmux);
                agent.id = format!("local/default/{id}");
                agent
            })
            .collect::<Vec<_>>();
        let mut list = AgentListState::default();
        list.reconcile_selection(&agents);
        list.select_visible(&agents, 2);

        agents.remove(2);
        list.reconcile_selection(&agents);

        assert_eq!(list.selected_snapshot_index(&agents), Some(2));
        assert_eq!(list.selected_id.as_deref(), Some("local/default/four"));
    }

    #[test]
    fn search_with_no_matches_cannot_activate_a_hidden_session() {
        let agents = vec![test_agent("Codex", Attention::Working, AgentOrigin::Tmux)];
        let mut list = AgentListState::default();
        list.reconcile_selection(&agents);
        list.enter_search();
        for character in "missing".chars() {
            list.push_query(character);
        }
        list.reconcile_selection(&agents);

        assert!(list.visible_indices(&agents).is_empty());
        assert_eq!(list.selected_snapshot_index(&agents), None);
    }

    #[test]
    fn redraws_when_search_changes_the_visible_rows() {
        let snapshot = Snapshot {
            agents: vec![
                test_agent("Codex", Attention::Working, AgentOrigin::Tmux),
                test_agent("Claude", Attention::Idle, AgentOrigin::Tmux),
            ],
            ..Snapshot::default()
        };
        let mut redraw = RedrawTracker::default();
        redraw.mark_rendered(RenderTopology::from_snapshot(&snapshot));

        assert!(redraw.needs_full_redraw(&RenderTopology::from_visible(&snapshot, &[1])));
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
    fn shortcut_keycaps_reuse_the_provider_gap_and_skip_subagents() {
        let mut agents = (1..=11)
            .map(|number| {
                let mut agent = test_agent("Codex", Attention::Idle, AgentOrigin::Tmux);
                agent.id = format!("local/default/session-{number:02}");
                agent.title = format!("session-{number:02}");
                agent
            })
            .collect::<Vec<_>>();
        let mut child = test_agent("Codex", Attention::Working, AgentOrigin::Terminal);
        child.id = "local/terminal/review".into();
        child.subagent = Some(SubagentInfo {
            parent_id: agents[0].id.clone(),
            started_at_ms: 1,
            finished_at_ms: None,
            name: Some("review".into()),
            thread_id: None,
        });
        agents.insert(1, child);
        let snapshot = Snapshot {
            agents,
            ..Snapshot::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 32)).unwrap();

        terminal
            .draw(|frame| render(frame, &snapshot, 0, "", 0))
            .unwrap();

        let keycap_x = 12;
        let second_session_row = 7;
        assert_eq!(terminal.backend().buffer()[(keycap_x - 1, 4)].symbol(), " ");
        assert_eq!(
            terminal.backend().buffer()[(keycap_x - 1, second_session_row)].bg,
            Color::Reset
        );
        assert_eq!(
            terminal.backend().buffer()[(keycap_x, second_session_row)].symbol(),
            "2"
        );
        assert_eq!(
            terminal.backend().buffer()[(keycap_x, second_session_row)].fg,
            Color::Rgb(145, 165, 171)
        );
        assert_eq!(
            terminal.backend().buffer()[(keycap_x, second_session_row)].bg,
            Color::Rgb(45, 45, 45)
        );
        assert_eq!(
            terminal.backend().buffer()[(keycap_x, 4)].fg,
            terminal.backend().buffer()[(3, 4)].fg
        );
        assert_eq!(
            terminal.backend().buffer()[(keycap_x, 4)].bg,
            terminal.backend().buffer()[(3, 4)].bg
        );
        assert_eq!(
            terminal.backend().buffer()[(keycap_x + 1, second_session_row)].bg,
            Color::Reset
        );
        assert_eq!(terminal.backend().buffer()[(keycap_x, 23)].symbol(), "0");
        assert_eq!(terminal.backend().buffer()[(keycap_x, 25)].symbol(), " ");
        assert_eq!(terminal.backend().buffer()[(keycap_x, 25)].bg, Color::Reset);
        let first_title_x = (0..80)
            .find(|x| terminal.backend().buffer()[(*x, 4)].symbol() == "s")
            .unwrap();
        let eleventh_title_x = (0..80)
            .find(|x| terminal.backend().buffer()[(*x, 25)].symbol() == "s")
            .unwrap();
        assert_eq!(first_title_x, eleventh_title_x);
        assert_eq!(first_title_x, 14);
        assert_eq!(terminal.backend().buffer()[(keycap_x, 6)].symbol(), " ");
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
            .draw(|frame| render_at(frame, &snapshot, 0, "", 5, 270_000))
            .unwrap();

        assert!(!row_text(&terminal, 4).contains("+1 agent"));
        assert!(row_text(&terminal, 6).contains("done  ·  4m 12s"));
        let done_x = row_text(&terminal, 6).find("done").unwrap() as u16;
        assert_eq!(terminal.backend().buffer()[(done_x, 6)].fg, Color::Green);
        assert_eq!(terminal.backend().buffer()[(89, 6)].bg, Color::Reset);

        terminal
            .draw(|frame| render_at(frame, &snapshot, 0, "", 5, 330_000))
            .unwrap();
        assert!(row_text(&terminal, 6).contains("done  ·  4m 12s"));
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
        let mut remote = test_agent("Codex", Attention::Done, AgentOrigin::Tmux);
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
        acknowledged_snapshot.agents[0].attention = Attention::Idle;
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
        let mut list = AgentListState::default();
        list.enter_search();
        for character in "done".chars() {
            list.push_query(character);
        }
        list.reconcile_selection(&snapshot.agents);
        assert_eq!(list.visible_indices(&snapshot.agents), vec![0]);
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
        let activation = activation.unwrap();
        assert_eq!(activation, Activation::Completed);
        assert_eq!(
            apply_activation_outcome(activation, &mut list, &snapshot.agents),
            None
        );
        assert_eq!(
            acknowledged.as_deref(),
            Some("remote/remote-mac/host/default/%1")
        );
        assert!(message.text().contains("acknowledged"));
        assert!(message.text().contains("focus unavailable"));
        assert!(!list.searching);
        assert!(list.query.is_empty());
        assert_eq!(list.visible_indices(&snapshot.agents), vec![0]);
        assert_eq!(list.selected_snapshot_index(&snapshot.agents), Some(0));
        assert_eq!(
            list.selected_id.as_deref(),
            Some("remote/remote-mac/host/default/%1")
        );
        assert!(
            !snapshot.agents[0]
                .goal
                .as_ref()
                .unwrap()
                .achievement_pending
        );
    }

    #[test]
    fn successful_focus_remains_completed_when_mark_used_is_unsupported() {
        const CHILD_ENV: &str = "TMUX_AGENT_ACTIVATION_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(successful_focus_with_unsupported_mark_used_child());
            return;
        }

        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ui::tests::successful_focus_remains_completed_when_mark_used_is_unsupported",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env_remove("TMUX")
            .status()
            .unwrap();

        assert!(status.success());
    }

    async fn successful_focus_with_unsupported_mark_used_child() {
        let directory = tempfile::tempdir().unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_name = format!("tmux-agent-ui-used-{}-{nonce}", std::process::id());
        let config = Config {
            tmux_args: vec!["-L".into(), socket_name.clone()],
            ..Config::default()
        };
        let paths = RuntimePaths {
            socket: directory.path().join("daemon.sock"),
            runners: directory.path().join("runners"),
            state: directory.path().join("state.json"),
            acknowledgements: directory.path().join("acknowledged.json"),
            log: directory.path().join("daemon.log"),
        };
        let listener = UnixListener::bind(&paths.socket).unwrap();
        let started = Command::new("tmux")
            .args([
                "-L",
                &socket_name,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "project-one",
            ])
            .status()
            .unwrap();
        assert!(started.success());
        let target = Command::new("tmux")
            .args([
                "-L",
                &socket_name,
                "display-message",
                "-p",
                "-t",
                "project-one",
                "#{window_id} #{pane_id}",
            ])
            .output()
            .unwrap();
        assert!(target.status.success());
        let target = String::from_utf8(target.stdout).unwrap();
        let mut target = target.split_whitespace();
        let window_id = target.next().unwrap().to_string();
        let pane_id = target.next().unwrap().to_string();
        let mut record = test_agent("Codex", Attention::Done, AgentOrigin::Tmux);
        record.window_id = window_id;
        record.pane_id = pane_id;
        let expected_id = record.id.clone();
        let mut acknowledged_snapshot = Snapshot {
            agents: vec![record.clone()],
            ..Snapshot::default()
        };
        acknowledged_snapshot.agents[0].attention = Attention::Idle;
        acknowledged_snapshot.agents[0].seen = true;
        let server = tokio::spawn(async move {
            let mut marked = None;
            let mut acknowledged = None;
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request = serde_json::from_str::<crate::model::IpcRequest>(&line).unwrap();
                let response = match request {
                    crate::model::IpcRequest::MarkUsed { target } => {
                        marked = Some(target);
                        crate::model::IpcResponse::Error {
                            message: "unsupported request: mark_used".into(),
                        }
                    }
                    crate::model::IpcRequest::Acknowledge { target } => {
                        acknowledged = Some(target);
                        crate::model::IpcResponse::Ack
                    }
                    crate::model::IpcRequest::Snapshot { .. } => {
                        crate::model::IpcResponse::Snapshot {
                            snapshot: acknowledged_snapshot.clone(),
                        }
                    }
                    request => panic!("unexpected request: {request:?}"),
                };
                let mut response = serde_json::to_vec(&response).unwrap();
                response.push(b'\n');
                writer.write_all(&response).await.unwrap();
            }
            (marked, acknowledged)
        });
        let tmux = Tmux::new(&config);
        let mut snapshot = Snapshot {
            agents: vec![record],
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

        let activation = activate_record(&context, &mut snapshot, 0, &mut message)
            .await
            .unwrap();
        let requests = tokio::time::timeout(Duration::from_millis(500), server).await;

        let _ = Command::new("tmux")
            .args(["-L", &socket_name, "kill-server"])
            .status();
        assert_eq!(activation, Activation::Completed);
        let (marked, acknowledged) = requests
            .expect("successful focus should finish optional usage and acknowledgement IPC")
            .unwrap();
        assert_eq!(marked.as_deref(), Some(expected_id.as_str()));
        assert_eq!(acknowledged.as_deref(), Some(expected_id.as_str()));
        assert_eq!(snapshot.agents[0].attention, Attention::Idle);
        assert!(snapshot.agents[0].seen);
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
