//! Helpers for rendering and navigating multi-agent state in the TUI.
//!
//! This module owns the shared presentation contracts for multi-agent history rows, `/agent` picker
//! entries, and the fast-switch keyboard shortcuts. Higher-level coordination, such as deciding
//! which thread becomes active or when a thread closes, stays in [`crate::app::App`].

use crate::exec_cell::spinner;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::render::line_utils::prefix_lines;
use crate::text_formatting::truncate_text;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabAgentInteractionEndEvent;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::CollabAgentSpawnEndEvent;
use codex_protocol::protocol::CollabAgentSpawnUpdateEvent;
use codex_protocol::protocol::CollabAgentStatusEntry;
use codex_protocol::protocol::CollabAgentToolCallMode;
use codex_protocol::protocol::CollabCloseEndEvent;
use codex_protocol::protocol::CollabResumeBeginEvent;
use codex_protocol::protocol::CollabResumeEndEvent;
use codex_protocol::protocol::CollabWaitingBeginEvent;
use codex_protocol::protocol::CollabWaitingEndEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
#[cfg(target_os = "macos")]
use crossterm::event::KeyEventKind;
#[cfg(target_os = "macos")]
use crossterm::event::KeyModifiers;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

const COLLAB_PROMPT_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentPickerThreadEntry {
    /// Human-friendly nickname shown in picker rows and footer labels.
    pub(crate) agent_nickname: Option<String>,
    /// Agent type shown in brackets when present, for example `worker`.
    pub(crate) agent_role: Option<String>,
    /// Whether the thread has emitted a close event and should render dimmed.
    pub(crate) is_closed: bool,
}

#[derive(Clone, Copy)]
struct AgentLabel<'a> {
    thread_id: Option<ThreadId>,
    nickname: Option<&'a str>,
    role: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequestSummary {
    pub(crate) model: String,
    pub(crate) reasoning_effort: ReasoningEffortConfig,
}

#[derive(Debug)]
pub(crate) struct BlockingSpawnCell {
    entries: Vec<BlockingSpawnEntry>,
    start_time: Instant,
    animations_enabled: bool,
}

#[derive(Debug)]
pub(crate) struct BlockingSpawnEntry {
    call_id: String,
    lines: Vec<Line<'static>>,
    is_finished: bool,
}

impl BlockingSpawnCell {
    pub(crate) fn new(entry: BlockingSpawnEntry, animations_enabled: bool) -> Self {
        Self {
            entries: vec![entry],
            start_time: Instant::now(),
            animations_enabled,
        }
    }

    pub(crate) fn contains_call_id(&self, call_id: &str) -> bool {
        self.entries.iter().any(|entry| entry.call_id == call_id)
    }

    pub(crate) fn upsert(&mut self, entry: BlockingSpawnEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|existing| existing.call_id == entry.call_id)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub(crate) fn finish(&mut self, call_id: &str, lines: Vec<Line<'static>>) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.call_id == call_id)
        else {
            return false;
        };
        entry.lines = lines;
        entry.is_finished = true;
        true
    }

    pub(crate) fn all_finished(&self) -> bool {
        self.entries.iter().all(|entry| entry.is_finished)
    }
}

impl HistoryCell for BlockingSpawnCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for entry in &self.entries {
            if entry.is_finished {
                lines.extend(entry.lines.clone());
            } else {
                lines.extend(lines_with_animated_bullet(
                    &entry.lines,
                    spinner(Some(self.start_time), self.animations_enabled),
                ));
            }
        }
        lines
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        self.entries
            .iter()
            .any(|entry| !entry.is_finished)
            .then(|| self.start_time.elapsed().as_millis() as u64 / 600)
    }
}

pub(crate) fn agent_picker_status_dot_spans(is_closed: bool) -> Vec<Span<'static>> {
    let dot = if is_closed {
        "•".into()
    } else {
        "•".green()
    };
    vec![dot, " ".into()]
}

pub(crate) fn format_agent_picker_item_name(
    agent_nickname: Option<&str>,
    agent_role: Option<&str>,
    is_primary: bool,
) -> String {
    if is_primary {
        return "Main [default]".to_string();
    }

    let agent_nickname = agent_nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let agent_role = agent_role.map(str::trim).filter(|role| !role.is_empty());
    match (agent_nickname, agent_role) {
        (Some(agent_nickname), Some(agent_role)) => format!("{agent_nickname} [{agent_role}]"),
        (Some(agent_nickname), None) => agent_nickname.to_string(),
        (None, Some(agent_role)) => format!("[{agent_role}]"),
        (None, None) => "Agent".to_string(),
    }
}

pub(crate) fn previous_agent_shortcut() -> crate::key_hint::KeyBinding {
    crate::key_hint::alt(KeyCode::Left)
}

pub(crate) fn next_agent_shortcut() -> crate::key_hint::KeyBinding {
    crate::key_hint::alt(KeyCode::Right)
}

/// Matches the canonical "previous agent" binding plus platform-specific fallbacks that keep agent
/// navigation working when enhanced key reporting is unavailable.
pub(crate) fn previous_agent_shortcut_matches(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    previous_agent_shortcut().is_press(key_event)
        || previous_agent_word_motion_fallback(key_event, allow_word_motion_fallback)
}

/// Matches the canonical "next agent" binding plus platform-specific fallbacks that keep agent
/// navigation working when enhanced key reporting is unavailable.
pub(crate) fn next_agent_shortcut_matches(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    next_agent_shortcut().is_press(key_event)
        || next_agent_word_motion_fallback(key_event, allow_word_motion_fallback)
}

#[cfg(target_os = "macos")]
fn previous_agent_word_motion_fallback(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    // Some terminals, especially on macOS, send Option+b/f as word-motion keys instead of
    // Option+arrow events unless enhanced keyboard reporting is enabled. Callers should only
    // enable this fallback when the composer is empty so draft editing retains the expected
    // word-wise motion behavior.
    allow_word_motion_fallback
        && matches!(
            key_event,
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }
        )
}

#[cfg(not(target_os = "macos"))]
fn previous_agent_word_motion_fallback(
    _key_event: KeyEvent,
    _allow_word_motion_fallback: bool,
) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn next_agent_word_motion_fallback(key_event: KeyEvent, allow_word_motion_fallback: bool) -> bool {
    // Some terminals, especially on macOS, send Option+b/f as word-motion keys instead of
    // Option+arrow events unless enhanced keyboard reporting is enabled. Callers should only
    // enable this fallback when the composer is empty so draft editing retains the expected
    // word-wise motion behavior.
    allow_word_motion_fallback
        && matches!(
            key_event,
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }
        )
}

#[cfg(not(target_os = "macos"))]
fn next_agent_word_motion_fallback(
    _key_event: KeyEvent,
    _allow_word_motion_fallback: bool,
) -> bool {
    false
}

pub(crate) fn spawn_end(
    ev: CollabAgentSpawnEndEvent,
    spawn_request: Option<&SpawnRequestSummary>,
) -> PlainHistoryCell {
    let CollabAgentSpawnEndEvent {
        call_id: _,
        sender_thread_id: _,
        new_thread_id,
        new_agent_nickname,
        new_agent_role,
        prompt,
        status: _,
        mode,
        tool_summary,
        ..
    } = ev;

    let title = match new_thread_id {
        Some(thread_id) => spawn_title(
            AgentLabel {
                thread_id: Some(thread_id),
                nickname: new_agent_nickname.as_deref(),
                role: new_agent_role.as_deref(),
            },
            spawn_request,
            mode,
        ),
        None => title_text("Agent spawn failed"),
    };

    collab_event(title, spawn_details(&prompt, tool_summary.as_ref()))
}

pub(crate) fn blocking_spawn_entry(ev: CollabAgentSpawnUpdateEvent) -> BlockingSpawnEntry {
    let CollabAgentSpawnUpdateEvent {
        call_id,
        sender_thread_id: _,
        new_thread_id,
        new_agent_nickname,
        new_agent_role,
        prompt,
        model,
        reasoning_effort,
        status: _,
        tool_summary,
    } = ev;
    let spawn_request = SpawnRequestSummary {
        model,
        reasoning_effort,
    };
    BlockingSpawnEntry {
        call_id,
        lines: collab_event_lines(
            title_spans_line_with_bullet(
                "•".dim(),
                spawn_title_spans(
                    "Starting Subagent ",
                    AgentLabel {
                        thread_id: Some(new_thread_id),
                        nickname: new_agent_nickname.as_deref(),
                        role: new_agent_role.as_deref(),
                    },
                    Some(&spawn_request),
                    CollabAgentToolCallMode::Blocking,
                ),
            ),
            spawn_details(&prompt, tool_summary.as_ref()),
        ),
        is_finished: false,
    }
}

pub(crate) fn interaction_end(ev: CollabAgentInteractionEndEvent) -> PlainHistoryCell {
    let CollabAgentInteractionEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        prompt,
        status: _,
    } = ev;

    let title = title_with_agent(
        "Sent input to",
        AgentLabel {
            thread_id: Some(receiver_thread_id),
            nickname: receiver_agent_nickname.as_deref(),
            role: receiver_agent_role.as_deref(),
        },
        /*spawn_request*/ None,
    );

    let mut details = Vec::new();
    if let Some(line) = prompt_line(&prompt) {
        details.push(line);
    }
    collab_event(title, details)
}

pub(crate) fn waiting_begin(ev: CollabWaitingBeginEvent) -> PlainHistoryCell {
    let CollabWaitingBeginEvent {
        sender_thread_id: _,
        receiver_thread_ids,
        receiver_agents,
        call_id: _,
    } = ev;
    let receiver_agents = merge_wait_receivers(&receiver_thread_ids, receiver_agents);

    let title = match receiver_agents.as_slice() {
        [receiver] => title_with_agent(
            "Waiting for",
            agent_label_from_ref(receiver),
            /*spawn_request*/ None,
        ),
        [] => title_text("Waiting for agents"),
        _ => title_text(format!("Waiting for {} agents", receiver_agents.len())),
    };

    let details = if receiver_agents.len() > 1 {
        receiver_agents
            .iter()
            .map(|receiver| agent_label_line(agent_label_from_ref(receiver)))
            .collect()
    } else {
        Vec::new()
    };

    collab_event(title, details)
}

pub(crate) fn waiting_end(ev: CollabWaitingEndEvent) -> PlainHistoryCell {
    let CollabWaitingEndEvent {
        call_id: _,
        sender_thread_id: _,
        agent_statuses,
        statuses,
    } = ev;
    let details = wait_complete_lines(&statuses, &agent_statuses);
    collab_event(title_text("Finished waiting"), details)
}

pub(crate) fn close_end(ev: CollabCloseEndEvent) -> PlainHistoryCell {
    let CollabCloseEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        status: _,
    } = ev;

    collab_event(
        title_with_agent(
            "Closed",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
            /*spawn_request*/ None,
        ),
        Vec::new(),
    )
}

pub(crate) fn resume_begin(ev: CollabResumeBeginEvent) -> PlainHistoryCell {
    let CollabResumeBeginEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
    } = ev;

    collab_event(
        title_with_agent(
            "Resuming",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
            /*spawn_request*/ None,
        ),
        Vec::new(),
    )
}

pub(crate) fn resume_end(ev: CollabResumeEndEvent) -> PlainHistoryCell {
    let CollabResumeEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        status,
    } = ev;

    collab_event(
        title_with_agent(
            "Resumed",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
            /*spawn_request*/ None,
        ),
        vec![status_summary_line(&status)],
    )
}

fn collab_event(title: Line<'static>, details: Vec<Line<'static>>) -> PlainHistoryCell {
    PlainHistoryCell::new(collab_event_lines(title, details))
}

fn collab_event_lines(title: Line<'static>, details: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![title];
    if !details.is_empty() {
        lines.extend(prefix_lines(details, "  └ ".dim(), "    ".into()));
    }
    lines
}

fn lines_with_animated_bullet(
    lines: &[Line<'static>],
    bullet: Span<'static>,
) -> Vec<Line<'static>> {
    let Some((first, rest)) = lines.split_first() else {
        return Vec::new();
    };
    let mut first_spans = first.spans.clone();
    if let Some(span) = first_spans.first_mut() {
        *span = bullet;
    }
    let mut animated = Vec::with_capacity(lines.len());
    animated.push(first_spans.into());
    animated.extend(rest.iter().cloned());
    animated
}

fn spawn_details(
    prompt: &str,
    tool_summary: Option<&codex_protocol::protocol::CollabAgentToolSummary>,
) -> Vec<Line<'static>> {
    if let Some(tool_summary) = tool_summary
        && !tool_summary.tools.is_empty()
    {
        return vec![Line::from(tool_summary_line(tool_summary))];
    }

    prompt_line(prompt).into_iter().collect()
}

fn tool_summary_line(tool_summary: &codex_protocol::protocol::CollabAgentToolSummary) -> String {
    let counts =
        tool_summary
            .tools
            .iter()
            .fold(ToolSummaryCounts::default(), |mut counts, tool| {
                counts.add(tool.name.as_str(), tool.count);
                counts
            });
    counts.into_parts().join(", ")
}

#[derive(Default)]
struct ToolSummaryCounts {
    read_files: u32,
    search_patterns: u32,
    listed_dirs: u32,
    execute_commands: u32,
    write_files: u32,
    edit_files: u32,
    delete_paths: u32,
    web_searches: u32,
    image_generations: u32,
    other_tools: u32,
}

impl ToolSummaryCounts {
    fn add(&mut self, name: &str, count: u32) {
        match name {
            "read_file" => self.read_files += count,
            "search_file" | "grep_file" | "glob_file" => self.search_patterns += count,
            "list_dir" => self.listed_dirs += count,
            "execute" | "shell" => self.execute_commands += count,
            "write" => self.write_files += count,
            "edit" => self.edit_files += count,
            "delete" => self.delete_paths += count,
            "web_search" => self.web_searches += count,
            "image_generation" => self.image_generations += count,
            _ => self.other_tools += count,
        }
    }

    fn into_parts(self) -> Vec<String> {
        let mut parts = Vec::new();
        push_count_part(
            &mut parts,
            "Search",
            self.search_patterns,
            "pattern",
            "patterns",
        );
        push_count_part(&mut parts, "Read", self.read_files, "file", "files");
        push_count_part(&mut parts, "List", self.listed_dirs, "dir", "dirs");
        push_count_part(
            &mut parts,
            "Execute",
            self.execute_commands,
            "command",
            "commands",
        );
        push_count_part(&mut parts, "Edit", self.edit_files, "file", "files");
        push_count_part(&mut parts, "Write", self.write_files, "file", "files");
        push_count_part(&mut parts, "Delete", self.delete_paths, "path", "paths");
        push_count_part(
            &mut parts,
            "Web Search",
            self.web_searches,
            "query",
            "queries",
        );
        push_count_part(
            &mut parts,
            "Image Generation",
            self.image_generations,
            "image",
            "images",
        );
        push_count_part(&mut parts, "Use", self.other_tools, "tool", "tools");
        parts
    }
}

fn push_count_part(parts: &mut Vec<String>, verb: &str, count: u32, singular: &str, plural: &str) {
    if count == 0 {
        return;
    }
    let noun = if count == 1 { singular } else { plural };
    parts.push(format!("{verb} {count} {noun}"));
}

fn title_text(title: impl Into<String>) -> Line<'static> {
    title_spans_line(vec![Span::from(title.into()).bold()])
}

fn title_with_agent(
    prefix: &str,
    agent: AgentLabel<'_>,
    spawn_request: Option<&SpawnRequestSummary>,
) -> Line<'static> {
    let mut spans = vec![Span::from(format!("{prefix} ")).bold()];
    spans.extend(agent_label_spans(agent));
    spans.extend(spawn_request_spans(spawn_request));
    title_spans_line(spans)
}

fn spawn_title(
    agent: AgentLabel<'_>,
    spawn_request: Option<&SpawnRequestSummary>,
    mode: CollabAgentToolCallMode,
) -> Line<'static> {
    let bullet = match mode {
        CollabAgentToolCallMode::Blocking => "•".green(),
        CollabAgentToolCallMode::Background => "•".dim(),
    };
    title_spans_line_with_bullet(
        bullet,
        spawn_title_spans("Started Subagent ", agent, spawn_request, mode),
    )
}

fn spawn_title_spans(
    prefix: &'static str,
    agent: AgentLabel<'_>,
    spawn_request: Option<&SpawnRequestSummary>,
    mode: CollabAgentToolCallMode,
) -> Vec<Span<'static>> {
    let mut spans = vec![prefix.bold()];
    spans.extend(agent_label_spans(agent));
    spans.extend(spawn_request_spans(spawn_request));
    if matches!(mode, CollabAgentToolCallMode::Background) {
        spans.push(" in background".dim());
    }
    spans
}

fn title_spans_line(spans: Vec<Span<'static>>) -> Line<'static> {
    title_spans_line_with_bullet("•".dim(), spans)
}

fn title_spans_line_with_bullet(
    bullet: Span<'static>,
    mut spans: Vec<Span<'static>>,
) -> Line<'static> {
    let mut title = Vec::with_capacity(spans.len() + 2);
    title.push(bullet);
    title.push(" ".into());
    title.append(&mut spans);
    title.into()
}

fn agent_label_from_ref(agent: &CollabAgentRef) -> AgentLabel<'_> {
    AgentLabel {
        thread_id: Some(agent.thread_id),
        nickname: agent.agent_nickname.as_deref(),
        role: agent.agent_role.as_deref(),
    }
}

fn agent_label_line(agent: AgentLabel<'_>) -> Line<'static> {
    agent_label_spans(agent).into()
}

fn agent_label_spans(agent: AgentLabel<'_>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let nickname = agent
        .nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let role = agent.role.map(str::trim).filter(|role| !role.is_empty());

    if let Some(nickname) = nickname {
        spans.push(Span::from(nickname.to_string()).cyan().bold());
    } else if let Some(thread_id) = agent.thread_id {
        spans.push(Span::from(thread_id.to_string()).cyan());
    } else {
        spans.push(Span::from("agent").cyan());
    }

    if let Some(role) = role {
        spans.push(Span::from(" ").dim());
        spans.push(Span::from(format!("[{role}]")));
    }

    spans
}

fn spawn_request_spans(spawn_request: Option<&SpawnRequestSummary>) -> Vec<Span<'static>> {
    let Some(spawn_request) = spawn_request else {
        return Vec::new();
    };

    let model = spawn_request.model.trim();
    if model.is_empty() && spawn_request.reasoning_effort == ReasoningEffortConfig::default() {
        return Vec::new();
    }

    let details = if model.is_empty() {
        format!("({})", spawn_request.reasoning_effort)
    } else {
        format!("({model} {})", spawn_request.reasoning_effort)
    };

    vec![Span::from(" ").dim(), Span::from(details).magenta()]
}

fn prompt_line(prompt: &str) -> Option<Line<'static>> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Line::from(Span::from(truncate_text(
            trimmed,
            COLLAB_PROMPT_PREVIEW_GRAPHEMES,
        ))))
    }
}

fn merge_wait_receivers(
    receiver_thread_ids: &[ThreadId],
    mut receiver_agents: Vec<CollabAgentRef>,
) -> Vec<CollabAgentRef> {
    if receiver_agents.is_empty() {
        return receiver_thread_ids
            .iter()
            .map(|thread_id| CollabAgentRef {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
            })
            .collect();
    }

    let mut seen = receiver_agents
        .iter()
        .map(|agent| agent.thread_id)
        .collect::<HashSet<_>>();
    for thread_id in receiver_thread_ids {
        if seen.insert(*thread_id) {
            receiver_agents.push(CollabAgentRef {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
            });
        }
    }
    receiver_agents
}

fn wait_complete_lines(
    statuses: &HashMap<ThreadId, AgentStatus>,
    agent_statuses: &[CollabAgentStatusEntry],
) -> Vec<Line<'static>> {
    if statuses.is_empty() && agent_statuses.is_empty() {
        return vec![Line::from(Span::from("No agents completed yet"))];
    }

    let entries = if agent_statuses.is_empty() {
        let mut entries = statuses
            .iter()
            .map(|(thread_id, status)| CollabAgentStatusEntry {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
                status: status.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
        entries
    } else {
        let mut entries = agent_statuses.to_vec();
        let seen = entries
            .iter()
            .map(|entry| entry.thread_id)
            .collect::<HashSet<_>>();
        let mut extras = statuses
            .iter()
            .filter(|(thread_id, _)| !seen.contains(thread_id))
            .map(|(thread_id, status)| CollabAgentStatusEntry {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
                status: status.clone(),
            })
            .collect::<Vec<_>>();
        extras.sort_by(|left, right| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
        entries.extend(extras);
        entries
    };

    entries
        .into_iter()
        .map(|entry| {
            let CollabAgentStatusEntry {
                thread_id,
                agent_nickname,
                agent_role,
                status,
            } = entry;
            let mut spans = agent_label_spans(AgentLabel {
                thread_id: Some(thread_id),
                nickname: agent_nickname.as_deref(),
                role: agent_role.as_deref(),
            });
            spans.push(Span::from(": ").dim());
            spans.extend(status_summary_spans(&status));
            spans.into()
        })
        .collect()
}

fn status_summary_line(status: &AgentStatus) -> Line<'static> {
    status_summary_spans(status).into()
}

fn status_summary_spans(status: &AgentStatus) -> Vec<Span<'static>> {
    match status {
        AgentStatus::PendingInit => vec![Span::from("Pending init").cyan()],
        AgentStatus::Running => vec![Span::from("Running").cyan().bold()],
        // Allow `.yellow()`
        #[allow(clippy::disallowed_methods)]
        AgentStatus::Interrupted => vec![Span::from("Interrupted").yellow()],
        AgentStatus::Completed(message) => {
            let mut spans = vec![Span::from("Completed").green()];
            if let Some(message) = message.as_ref() {
                let message_preview = truncate_text(
                    &message.split_whitespace().collect::<Vec<_>>().join(" "),
                    COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES,
                );
                if !message_preview.is_empty() {
                    spans.push(Span::from(" - ").dim());
                    spans.push(Span::from(message_preview));
                }
            }
            spans
        }
        AgentStatus::Errored(error) => {
            let mut spans = vec![Span::from("Error").red()];
            let error_preview = truncate_text(
                &error.split_whitespace().collect::<Vec<_>>().join(" "),
                COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES,
            );
            if !error_preview.is_empty() {
                spans.push(Span::from(" - ").dim());
                spans.push(Span::from(error_preview));
            }
            spans
        }
        AgentStatus::Shutdown => vec![Span::from("Shutdown")],
        AgentStatus::NotFound => vec![Span::from("Not found").red()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::HistoryCell;
    #[cfg(target_os = "macos")]
    use crossterm::event::KeyEvent;
    #[cfg(target_os = "macos")]
    use crossterm::event::KeyModifiers;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use ratatui::style::Modifier;

    #[test]
    fn collab_events_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let bob_id = ThreadId::from_string("00000000-0000-0000-0000-000000000003")
            .expect("valid bob thread id");

        let spawn = spawn_end(
            CollabAgentSpawnEndEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: Some(robie_id),
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: "Compute 11! and reply with just the integer result.".to_string(),
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
                status: AgentStatus::PendingInit,
                mode: CollabAgentToolCallMode::Background,
                tool_summary: None,
            },
            Some(&SpawnRequestSummary {
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
            }),
        );

        let send = interaction_end(CollabAgentInteractionEndEvent {
            call_id: "call-send".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            prompt: "Please continue and return the answer only.".to_string(),
            status: AgentStatus::Running,
        });

        let waiting = waiting_begin(CollabWaitingBeginEvent {
            sender_thread_id,
            receiver_thread_ids: vec![robie_id],
            receiver_agents: vec![CollabAgentRef {
                thread_id: robie_id,
                agent_nickname: Some("Robie".to_string()),
                agent_role: Some("explorer".to_string()),
            }],
            call_id: "call-wait".to_string(),
        });

        let mut statuses = HashMap::new();
        statuses.insert(
            robie_id,
            AgentStatus::Completed(Some("39916800".to_string())),
        );
        statuses.insert(bob_id, AgentStatus::Errored("tool timeout".to_string()));
        let finished = waiting_end(CollabWaitingEndEvent {
            sender_thread_id,
            call_id: "call-wait".to_string(),
            agent_statuses: vec![
                CollabAgentStatusEntry {
                    thread_id: robie_id,
                    agent_nickname: Some("Robie".to_string()),
                    agent_role: Some("explorer".to_string()),
                    status: AgentStatus::Completed(Some("39916800".to_string())),
                },
                CollabAgentStatusEntry {
                    thread_id: bob_id,
                    agent_nickname: Some("Bob".to_string()),
                    agent_role: Some("worker".to_string()),
                    status: AgentStatus::Errored("tool timeout".to_string()),
                },
            ],
            statuses,
        });

        let close = close_end(CollabCloseEndEvent {
            call_id: "call-close".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            status: AgentStatus::Completed(Some("39916800".to_string())),
        });

        let snapshot = [spawn, send, waiting, finished, close]
            .iter()
            .map(|cell| cell_to_text(cell))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_snapshot!("collab_agent_transcript", snapshot);
    }

    #[test]
    fn blocking_spawn_summary_lists_only_used_tools() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");

        let cell = spawn_end(
            CollabAgentSpawnEndEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: Some(robie_id),
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("worker".to_string()),
                prompt: "Inspect only the target file.".to_string(),
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::Medium,
                status: AgentStatus::Completed(Some("done".to_string())),
                mode: CollabAgentToolCallMode::Blocking,
                tool_summary: Some(codex_protocol::protocol::CollabAgentToolSummary {
                    tools: vec![codex_protocol::protocol::CollabAgentToolSummaryEntry {
                        name: "read_file".to_string(),
                        count: 1,
                    }],
                    output: vec!["Read codex-rs/tui/src/multi_agents.rs".to_string()],
                }),
            },
            Some(&SpawnRequestSummary {
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::Medium,
            }),
        );

        assert_snapshot!(
            "blocking_spawn_summary_lists_only_used_tools",
            cell_to_text(&cell)
        );
    }

    #[test]
    fn blocking_spawn_update_renders_live_tool_progress() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");

        let cell = BlockingSpawnCell::new(
            blocking_spawn_entry(CollabAgentSpawnUpdateEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: robie_id,
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: "Explore the codebase.".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
                status: AgentStatus::Running,
                tool_summary: Some(codex_protocol::protocol::CollabAgentToolSummary {
                    tools: vec![
                        codex_protocol::protocol::CollabAgentToolSummaryEntry {
                            name: "grep_file".to_string(),
                            count: 1,
                        },
                        codex_protocol::protocol::CollabAgentToolSummaryEntry {
                            name: "read_file".to_string(),
                            count: 1,
                        },
                    ],
                    output: vec![
                        "Search CollabAgentSpawn".to_string(),
                        "Read codex-rs/tui/src/chatwidget.rs".to_string(),
                    ],
                }),
            }),
            /*animations_enabled*/ false,
        );

        assert_snapshot!(
            "blocking_spawn_update_renders_live_tool_progress",
            cell_to_text(&cell)
        );
    }

    #[test]
    fn blocking_spawn_update_animates_when_enabled() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");

        let cell = BlockingSpawnCell::new(
            blocking_spawn_entry(CollabAgentSpawnUpdateEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: robie_id,
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: "Explore the codebase.".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
                status: AgentStatus::Running,
                tool_summary: None,
            }),
            /*animations_enabled*/ true,
        );

        assert!(cell.transcript_animation_tick().is_some());
    }

    #[test]
    fn blocking_spawn_end_uses_green_status_dot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let cell = spawn_end(
            CollabAgentSpawnEndEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: Some(robie_id),
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("worker".to_string()),
                prompt: String::new(),
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::Medium,
                status: AgentStatus::Completed(Some("done".to_string())),
                mode: CollabAgentToolCallMode::Blocking,
                tool_summary: None,
            },
            Some(&SpawnRequestSummary {
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::Medium,
            }),
        );

        let lines = cell.display_lines(/*width*/ 200);
        let title = &lines[0];
        assert_eq!(title.spans[0].content.as_ref(), "•");
        assert_eq!(title.spans[0].style.fg, Some(Color::Green));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn agent_shortcut_matches_option_arrow_word_motion_fallbacks_only_when_allowed() {
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ true,
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ true,
        ));
        assert!(!previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(!next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn agent_shortcut_matches_option_arrows_only() {
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Left, crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Right, crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(!previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(!next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
    }

    #[test]
    fn title_styles_nickname_and_role() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let cell = spawn_end(
            CollabAgentSpawnEndEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: Some(robie_id),
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: String::new(),
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
                status: AgentStatus::PendingInit,
                mode: CollabAgentToolCallMode::Background,
                tool_summary: None,
            },
            Some(&SpawnRequestSummary {
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
            }),
        );

        let lines = cell.display_lines(/*width*/ 200);
        let title = &lines[0];
        assert_eq!(title.spans[3].content.as_ref(), "Robie");
        assert_eq!(title.spans[3].style.fg, Some(Color::Cyan));
        assert!(title.spans[3].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(title.spans[5].content.as_ref(), "[explorer]");
        assert_eq!(title.spans[5].style.fg, None);
        assert!(!title.spans[5].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(title.spans[7].content.as_ref(), "(gpt-5 high)");
        assert_eq!(title.spans[7].style.fg, Some(Color::Magenta));
    }

    #[test]
    fn collab_resume_interrupted_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");

        let cell = resume_end(CollabResumeEndEvent {
            call_id: "call-resume".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            status: AgentStatus::Interrupted,
        });

        assert_snapshot!("collab_resume_interrupted", cell_to_text(&cell));
    }

    fn cell_to_text(cell: &dyn HistoryCell) -> String {
        cell.display_lines(/*width*/ 200)
            .iter()
            .map(line_to_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_to_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }
}
