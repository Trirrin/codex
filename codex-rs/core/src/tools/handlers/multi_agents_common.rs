use crate::agent::AgentStatus;
use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_features::Feature;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::CollabAgentSpawnUpdateEvent;
use codex_protocol::protocol::CollabAgentStatusEntry;
use codex_protocol::protocol::CollabAgentToolCallMode;
use codex_protocol::protocol::CollabAgentToolSummary;
use codex_protocol::protocol::CollabAgentToolSummaryEntry;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch::Receiver;
use tokio::time::Instant;
use tokio::time::timeout_at;

/// Minimum wait timeout to prevent tight polling loops from burning CPU.
pub(crate) const MIN_WAIT_TIMEOUT_MS: i64 = 10_000;
pub(crate) const DEFAULT_WAIT_TIMEOUT_MS: i64 = 30_000;
pub(crate) const MAX_WAIT_TIMEOUT_MS: i64 = 3600 * 1000;
pub(crate) const DEFAULT_BLOCKING_AGENT_TIMEOUT_MS: i64 = MAX_WAIT_TIMEOUT_MS;
const BLOCKING_AGENT_PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub(crate) enum AgentToolMode {
    Blocking,
    #[default]
    Background,
}

pub(crate) fn collab_tool_call_mode(mode: AgentToolMode) -> CollabAgentToolCallMode {
    match mode {
        AgentToolMode::Blocking => CollabAgentToolCallMode::Blocking,
        AgentToolMode::Background => CollabAgentToolCallMode::Background,
    }
}

pub(crate) async fn collab_agent_tool_summary(
    session: Arc<Session>,
    thread_id: ThreadId,
) -> Option<CollabAgentToolSummary> {
    let history = session
        .services
        .agent_control
        .clone_agent_history(thread_id)
        .await
        .ok()?;
    let summary = collab_agent_tool_summary_from_history(&history);
    (!summary.tools.is_empty()).then_some(summary)
}

fn collab_agent_tool_summary_from_history(history: &[ResponseItem]) -> CollabAgentToolSummary {
    let outputs = history
        .iter()
        .filter_map(tool_call_output_text)
        .collect::<HashMap<_, _>>();
    let mut summary = CollabAgentToolSummary::default();

    for item in history {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                increment_tool_count(&mut summary.tools, name);
                summary.output.push(function_call_summary_line(
                    name,
                    arguments,
                    outputs.get(call_id),
                ));
            }
            ResponseItem::CustomToolCall { name, input, .. } => {
                increment_tool_count(&mut summary.tools, name);
                summary
                    .output
                    .push(generic_tool_summary_line(name, Some(input.as_str())));
            }
            ResponseItem::ToolSearchCall {
                call_id: _,
                execution,
                arguments,
                ..
            } => {
                increment_tool_count(&mut summary.tools, execution);
                summary
                    .output
                    .push(generic_json_tool_summary_line(execution, arguments));
            }
            ResponseItem::LocalShellCall { action, .. } => {
                increment_tool_count(&mut summary.tools, "shell");
                summary.output.push(local_shell_summary_line(action));
            }
            ResponseItem::WebSearchCall { .. } => {
                increment_tool_count(&mut summary.tools, "web_search");
                summary.output.push("Web Search".to_string());
            }
            ResponseItem::ImageGenerationCall { .. } => {
                increment_tool_count(&mut summary.tools, "image_generation");
                summary.output.push("Image Generation".to_string());
            }
            ResponseItem::Message { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => {}
        }
    }

    summary
}

fn increment_tool_count(entries: &mut Vec<CollabAgentToolSummaryEntry>, name: &str) {
    if let Some(entry) = entries.iter_mut().find(|entry| entry.name == name) {
        entry.count += 1;
        return;
    }
    entries.push(CollabAgentToolSummaryEntry {
        name: name.to_string(),
        count: 1,
    });
}

fn function_call_summary_line(name: &str, arguments: &str, output: Option<&String>) -> String {
    match name {
        "read_file" => argument_string(arguments, "path")
            .map(|path| format!("Read {path}"))
            .unwrap_or_else(|| "Read".to_string()),
        "search_file" | "grep_file" => argument_string(arguments, "query")
            .or_else(|| argument_string(arguments, "pattern"))
            .map(|pattern| format!("Search {pattern}"))
            .unwrap_or_else(|| "Search".to_string()),
        "glob_file" => argument_string(arguments, "pattern")
            .map(|pattern| format!("Glob {pattern}"))
            .unwrap_or_else(|| "Glob".to_string()),
        "delete" => argument_string(arguments, "path")
            .map(|path| format!("Delete {path}"))
            .unwrap_or_else(|| "Delete".to_string()),
        "list_dir" => argument_string(arguments, "dir_path")
            .map(|path| format!("List {path}"))
            .unwrap_or_else(|| "List".to_string()),
        "execute" => argument_string(arguments, "cmd")
            .map(|cmd| format!("Execute {cmd}"))
            .unwrap_or_else(|| "Execute".to_string()),
        "edit" => {
            let path = argument_string(arguments, "path").unwrap_or_else(|| "file".to_string());
            let (added, removed) = output
                .map(String::as_str)
                .map(diff_line_counts)
                .unwrap_or_default();
            format!("Edit {path}(+{added} -{removed})")
        }
        "write" => {
            let path = argument_string(arguments, "path").unwrap_or_else(|| "file".to_string());
            let added = argument_string(arguments, "content")
                .map(|content| content.lines().count())
                .unwrap_or_default();
            format!("Write {path}(+{added} -0)")
        }
        _ => generic_function_call_summary_line(name, arguments),
    }
}

fn generic_function_call_summary_line(name: &str, arguments: &str) -> String {
    let label = short_tool_label(name);
    let detail = serde_json::from_str::<JsonValue>(arguments)
        .ok()
        .and_then(|arguments| first_summary_argument(&arguments));
    match detail {
        Some(detail) if !detail.is_empty() => format!("{label} {detail}"),
        Some(_) | None => label,
    }
}

fn generic_json_tool_summary_line(name: &str, arguments: &JsonValue) -> String {
    let label = short_tool_label(name);
    match first_summary_argument(arguments) {
        Some(detail) if !detail.is_empty() => format!("{label} {detail}"),
        Some(_) | None => label,
    }
}

fn generic_tool_summary_line(name: &str, detail: Option<&str>) -> String {
    let label = short_tool_label(name);
    match detail.map(truncate_summary_value) {
        Some(detail) if !detail.is_empty() => format!("{label} {detail}"),
        Some(_) | None => label,
    }
}

fn local_shell_summary_line(action: &codex_protocol::models::LocalShellAction) -> String {
    match action {
        codex_protocol::models::LocalShellAction::Exec(action) => {
            generic_tool_summary_line("shell", Some(&action.command.join(" ")))
        }
    }
}

fn first_summary_argument(arguments: &JsonValue) -> Option<String> {
    [
        "path",
        "dir_path",
        "cmd",
        "pattern",
        "query",
        "command",
        "message",
        "task_name",
    ]
    .into_iter()
    .find_map(|key| argument_value(arguments, key))
}

fn argument_value(arguments: &JsonValue, key: &str) -> Option<String> {
    let value = arguments.get(key)?;
    match value {
        JsonValue::String(value) => Some(truncate_summary_value(value)),
        JsonValue::Number(_) | JsonValue::Bool(_) => Some(value.to_string()),
        JsonValue::Array(_) | JsonValue::Object(_) | JsonValue::Null => None,
    }
}

fn truncate_summary_value(value: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn short_tool_label(name: &str) -> String {
    match name {
        "read_file" => "Read".to_string(),
        "write" => "Write".to_string(),
        "edit" => "Edit".to_string(),
        "delete" => "Delete".to_string(),
        "list_dir" => "List".to_string(),
        "search_file" | "grep_file" => "Search".to_string(),
        "glob_file" => "Glob".to_string(),
        "execute" | "shell" => "Execute".to_string(),
        "web_search" => "Web".to_string(),
        "image_generation" => "Image".to_string(),
        "view_image" => "View".to_string(),
        "update_plan" => "Plan".to_string(),
        "spawn_agent" => "Spawn".to_string(),
        "send_input" => "Send".to_string(),
        "resume_agent" => "Resume".to_string(),
        "wait_agent" => "Wait".to_string(),
        "close_agent" => "Close".to_string(),
        _ => format_tool_name(name),
    }
}

fn format_tool_name(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn tool_call_output_text(item: &ResponseItem) -> Option<(String, String)> {
    let (call_id, output) = match item {
        ResponseItem::FunctionCallOutput { call_id, output } => (call_id, output),
        ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => (call_id, output),
        _ => return None,
    };
    let text = match &output.body {
        FunctionCallOutputBody::Text(text) => text.clone(),
        FunctionCallOutputBody::ContentItems(items) => {
            FunctionCallOutputBody::ContentItems(items.clone()).to_text()?
        }
    };
    Some((call_id.clone(), text))
}

fn argument_string(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<JsonValue>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(ToString::to_string)
}

fn diff_line_counts(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+++") || line.starts_with("---") {
            (added, removed)
        } else if line.starts_with('+') {
            (added + 1, removed)
        } else if line.starts_with('-') {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

pub(crate) fn function_arguments(payload: ToolPayload) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string(),
        )),
    }
}

pub(crate) fn tool_output_json_text<T>(value: &T, tool_name: &str) -> String
where
    T: Serialize,
{
    serde_json::to_string(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}")).to_string()
    })
}

pub(crate) fn tool_output_response_item<T>(
    call_id: &str,
    payload: &ToolPayload,
    value: &T,
    success: Option<bool>,
    tool_name: &str,
) -> ResponseInputItem
where
    T: Serialize,
{
    FunctionToolOutput::from_text(tool_output_json_text(value, tool_name), success)
        .to_response_item(call_id, payload)
}

pub(crate) fn tool_output_code_mode_result<T>(value: &T, tool_name: &str) -> JsonValue
where
    T: Serialize,
{
    serde_json::to_value(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}"))
    })
}

pub(crate) fn build_wait_agent_statuses(
    statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_agents: &[CollabAgentRef],
) -> Vec<CollabAgentStatusEntry> {
    if statuses.is_empty() {
        return Vec::new();
    }

    let mut entries = Vec::with_capacity(statuses.len());
    let mut seen = HashMap::with_capacity(receiver_agents.len());
    for receiver_agent in receiver_agents {
        seen.insert(receiver_agent.thread_id, ());
        if let Some(status) = statuses.get(&receiver_agent.thread_id) {
            entries.push(CollabAgentStatusEntry {
                thread_id: receiver_agent.thread_id,
                agent_nickname: receiver_agent.agent_nickname.clone(),
                agent_role: receiver_agent.agent_role.clone(),
                status: status.clone(),
            });
        }
    }

    let mut extras = statuses
        .iter()
        .filter(|(thread_id, _)| !seen.contains_key(thread_id))
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
}

pub(crate) fn collab_spawn_error(err: CodexErr) -> FunctionCallError {
    match err {
        CodexErr::UnsupportedOperation(message) if message == "thread manager dropped" => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        CodexErr::UnsupportedOperation(message) => FunctionCallError::RespondToModel(message),
        err => FunctionCallError::RespondToModel(format!("collab spawn failed: {err}")),
    }
}

pub(crate) fn collab_agent_error(agent_id: ThreadId, err: CodexErr) -> FunctionCallError {
    match err {
        CodexErr::ThreadNotFound(id) => {
            FunctionCallError::RespondToModel(format!("agent with id {id} not found"))
        }
        CodexErr::InternalAgentDied => {
            FunctionCallError::RespondToModel(format!("agent with id {agent_id} is closed"))
        }
        CodexErr::UnsupportedOperation(_) => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        err => FunctionCallError::RespondToModel(format!("collab tool failed: {err}")),
    }
}

#[derive(Clone)]
pub(crate) struct SpawnProgress {
    pub(crate) call_id: String,
    pub(crate) thread_id: ThreadId,
    pub(crate) nickname: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) prompt: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: ReasoningEffort,
}

pub(crate) fn sync_spawn_final_status_on_completion(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    progress: SpawnProgress,
) {
    tokio::spawn(async move {
        let status = match session
            .services
            .agent_control
            .subscribe_status(progress.thread_id)
            .await
        {
            Ok(status_rx) => wait_for_final_status(session.clone(), progress.thread_id, status_rx)
                .await
                .map(|(_, status)| status),
            Err(_) => Some(
                session
                    .services
                    .agent_control
                    .get_status(progress.thread_id)
                    .await,
            ),
        };
        let Some(status) = status else {
            return;
        };
        if !crate::agent::status::is_final(&status) {
            return;
        }

        let tool_summary = collab_agent_tool_summary(session.clone(), progress.thread_id).await;
        emit_spawn_update(&session, &turn, &progress, status, tool_summary).await;
    });
}

pub(crate) async fn wait_for_blocking_spawn_final_status(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    progress: SpawnProgress,
    timeout_ms: i64,
) -> AgentStatus {
    let timeout_ms = timeout_ms.clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    match session
        .services
        .agent_control
        .subscribe_status(progress.thread_id)
        .await
    {
        Ok(status_rx) => {
            match timeout_at(
                deadline,
                wait_for_blocking_spawn_final_status_inner(
                    session.clone(),
                    turn,
                    progress.clone(),
                    status_rx,
                ),
            )
            .await
            .ok()
            .flatten()
            {
                Some(status) => status,
                None => {
                    session
                        .services
                        .agent_control
                        .get_status(progress.thread_id)
                        .await
                }
            }
        }
        Err(_) => {
            session
                .services
                .agent_control
                .get_status(progress.thread_id)
                .await
        }
    }
}

async fn wait_for_blocking_spawn_final_status_inner(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    progress: SpawnProgress,
    mut status_rx: Receiver<AgentStatus>,
) -> Option<AgentStatus> {
    let mut status = status_rx.borrow().clone();
    let mut last_summary = None;
    emit_spawn_update(&session, &turn, &progress, status.clone(), None).await;
    if crate::agent::status::is_final(&status) {
        return Some(status);
    }

    let mut interval = tokio::time::interval(BLOCKING_AGENT_PROGRESS_UPDATE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = status_rx.changed() => {
                if changed.is_err() {
                    let latest = session.services.agent_control.get_status(progress.thread_id).await;
                    return crate::agent::status::is_final(&latest).then_some(latest);
                }
                status = status_rx.borrow().clone();
                if crate::agent::status::is_final(&status) {
                    return Some(status);
                }
            }
            _ = interval.tick() => {
                let summary = collab_agent_tool_summary(session.clone(), progress.thread_id).await;
                if summary != last_summary {
                    last_summary = summary.clone();
                    emit_spawn_update(&session, &turn, &progress, status.clone(), summary).await;
                }
            }
        }
    }
}

async fn emit_spawn_update(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    progress: &SpawnProgress,
    status: AgentStatus,
    tool_summary: Option<CollabAgentToolSummary>,
) {
    session
        .send_event(
            turn,
            CollabAgentSpawnUpdateEvent {
                call_id: progress.call_id.clone(),
                sender_thread_id: session.conversation_id,
                new_thread_id: progress.thread_id,
                new_agent_nickname: progress.nickname.clone(),
                new_agent_role: progress.role.clone(),
                prompt: progress.prompt.clone(),
                model: progress.model.clone(),
                reasoning_effort: progress.reasoning_effort,
                status,
                tool_summary,
            }
            .into(),
        )
        .await;
}

pub(crate) async fn wait_for_agent_final_status(
    session: Arc<Session>,
    thread_id: ThreadId,
    timeout_ms: i64,
) -> AgentStatus {
    let timeout_ms = timeout_ms.clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    match session
        .services
        .agent_control
        .subscribe_status(thread_id)
        .await
    {
        Ok(status_rx) => {
            match timeout_at(
                deadline,
                wait_for_final_status(session.clone(), thread_id, status_rx),
            )
            .await
            .ok()
            .flatten()
            {
                Some((_, status)) => status,
                None => session.services.agent_control.get_status(thread_id).await,
            }
        }
        Err(_) => session.services.agent_control.get_status(thread_id).await,
    }
}

async fn wait_for_final_status(
    session: Arc<Session>,
    thread_id: ThreadId,
    mut status_rx: Receiver<AgentStatus>,
) -> Option<(ThreadId, AgentStatus)> {
    let mut status = status_rx.borrow().clone();
    if crate::agent::status::is_final(&status) {
        return Some((thread_id, status));
    }

    loop {
        if status_rx.changed().await.is_err() {
            let latest = session.services.agent_control.get_status(thread_id).await;
            return crate::agent::status::is_final(&latest).then_some((thread_id, latest));
        }
        status = status_rx.borrow().clone();
        if crate::agent::status::is_final(&status) {
            return Some((thread_id, status));
        }
    }
}

pub(crate) fn thread_spawn_source(
    parent_thread_id: ThreadId,
    parent_session_source: &SessionSource,
    depth: i32,
    agent_role: Option<&str>,
    task_name: Option<String>,
) -> Result<SessionSource, FunctionCallError> {
    let agent_path = task_name
        .as_deref()
        .map(|task_name| {
            parent_session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root)
                .join(task_name)
                .map_err(FunctionCallError::RespondToModel)
        })
        .transpose()?;
    Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth,
        agent_path,
        agent_nickname: None,
        agent_role: agent_role.map(str::to_string),
    }))
}

pub(crate) fn parse_collab_input(
    message: Option<String>,
    items: Option<Vec<UserInput>>,
) -> Result<Op, FunctionCallError> {
    match (message, items) {
        (Some(_), Some(_)) => Err(FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string(),
        )),
        (None, None) => Err(FunctionCallError::RespondToModel(
            "Provide one of: message or items".to_string(),
        )),
        (Some(message), None) => {
            if message.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Empty message can't be sent to an agent".to_string(),
                ));
            }
            Ok(vec![UserInput::Text {
                text: message,
                text_elements: Vec::new(),
            }]
            .into())
        }
        (None, Some(items)) => {
            if items.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Items can't be empty".to_string(),
                ));
            }
            Ok(items.into())
        }
    }
}

/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// runtime-owned fields carried on `turn`, including model selection, reasoning settings,
/// approval policy, sandbox, and cwd. Role-specific overrides are layered after this step;
/// skipping this helper and cloning stale config state directly can send the child agent out with
/// the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    turn: &TurnContext,
) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    config.base_instructions = Some(base_instructions.text.clone());
    Ok(config)
}

pub(crate) fn build_agent_resume_config(
    turn: &TurnContext,
    child_depth: i32,
) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    apply_spawn_agent_overrides(&mut config, child_depth);
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, FunctionCallError> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    config.model = Some(turn.model_info.slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort
        .or(turn.model_info.default_reasoning_level);
    config.model_reasoning_summary = Some(turn.reasoning_summary);
    config.developer_instructions = turn.developer_instructions.clone();
    config.compact_prompt = turn.compact_prompt.clone();
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    Ok(config)
}

pub(crate) fn reject_full_fork_spawn_overrides(
    agent_type: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), FunctionCallError> {
    if agent_type.is_some() || model.is_some() || reasoning_effort.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "Full-history forked agents inherit the parent agent type, model, and reasoning effort; omit agent_type, model, and reasoning_effort, or spawn without a full-history fork.".to_string(),
        ));
    }
    Ok(())
}

/// Copies runtime-only turn state onto a child config before it is handed to `AgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale
/// can make a child agent disagree with its parent about approval policy, cwd, or sandboxing.
pub(crate) fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
) -> Result<(), FunctionCallError> {
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy.value())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("approval_policy is invalid: {err}"))
        })?;
    config.permissions.shell_environment_policy = turn.shell_environment_policy.clone();
    config.codex_linux_sandbox_exe = turn.codex_linux_sandbox_exe.clone();
    config.cwd = turn.cwd.clone();
    config
        .permissions
        .set_permission_profile(turn.permission_profile())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("permission_profile is invalid: {err}"))
        })?;
    Ok(())
}

pub(crate) fn apply_spawn_agent_overrides(config: &mut Config, child_depth: i32) {
    if child_depth >= config.agent_max_depth && !config.features.enabled(Feature::MultiAgentV2) {
        let _ = config.features.disable(Feature::SpawnCsv);
        let _ = config.features.disable(Feature::Collab);
    }
}

pub(crate) async fn apply_requested_spawn_agent_model_overrides(
    session: &Session,
    turn: &TurnContext,
    config: &mut Config,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), FunctionCallError> {
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let available_models = session
            .services
            .models_manager
            .list_models(RefreshStrategy::Offline)
            .await;
        let selected_model_name = find_spawn_agent_model_name(&available_models, requested_model)?;
        let selected_model_info = session
            .services
            .models_manager
            .get_model_info(&selected_model_name, &config.to_models_manager_config())
            .await;

        config.model = Some(selected_model_name.clone());
        if let Some(reasoning_effort) = requested_reasoning_effort {
            validate_spawn_agent_reasoning_effort(
                &selected_model_name,
                &selected_model_info.supported_reasoning_levels,
                reasoning_effort,
            )?;
            config.model_reasoning_effort = Some(reasoning_effort);
        } else {
            config.model_reasoning_effort = selected_model_info.default_reasoning_level;
        }

        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        validate_spawn_agent_reasoning_effort(
            &turn.model_info.slug,
            &turn.model_info.supported_reasoning_levels,
            reasoning_effort,
        )?;
        config.model_reasoning_effort = Some(reasoning_effort);
    }

    Ok(())
}

fn find_spawn_agent_model_name(
    available_models: &[codex_protocol::openai_models::ModelPreset],
    requested_model: &str,
) -> Result<String, FunctionCallError> {
    available_models
        .iter()
        .find(|model| model.model == requested_model)
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            FunctionCallError::RespondToModel(format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            ))
        })
}

fn validate_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[ReasoningEffortPreset],
    requested_reasoning_effort: ReasoningEffort,
) -> Result<(), FunctionCallError> {
    if supported_reasoning_levels
        .iter()
        .any(|preset| preset.effort == requested_reasoning_effort)
    {
        return Ok(());
    }

    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(FunctionCallError::RespondToModel(format!(
        "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn collab_agent_tool_summary_lists_only_tools_used() {
        let history = vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                namespace: None,
                arguments: r#"{"path":"codex-rs/tui/src/multi_agents.rs"}"#.to_string(),
                call_id: "read-call".to_string(),
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "list_dir".to_string(),
                namespace: None,
                arguments: r#"{"dir_path":"codex-rs/tui"}"#.to_string(),
                call_id: "list-call".to_string(),
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "execute".to_string(),
                namespace: None,
                arguments: r#"{"cmd":"cargo test -p codex-tui"}"#.to_string(),
                call_id: "execute-call".to_string(),
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "fetch_ticket".to_string(),
                namespace: None,
                arguments: r#"{"query":"COD-123"}"#.to_string(),
                call_id: "ticket-call".to_string(),
            },
            ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "custom-call".to_string(),
                name: "index_symbols".to_string(),
                input: "crate:codex-tui".to_string(),
            },
        ];

        let summary = collab_agent_tool_summary_from_history(&history);

        assert_eq!(
            summary,
            CollabAgentToolSummary {
                tools: vec![
                    CollabAgentToolSummaryEntry {
                        name: "read_file".to_string(),
                        count: 1,
                    },
                    CollabAgentToolSummaryEntry {
                        name: "list_dir".to_string(),
                        count: 1,
                    },
                    CollabAgentToolSummaryEntry {
                        name: "execute".to_string(),
                        count: 1,
                    },
                    CollabAgentToolSummaryEntry {
                        name: "fetch_ticket".to_string(),
                        count: 1,
                    },
                    CollabAgentToolSummaryEntry {
                        name: "index_symbols".to_string(),
                        count: 1,
                    },
                ],
                output: vec![
                    "Read codex-rs/tui/src/multi_agents.rs".to_string(),
                    "List codex-rs/tui".to_string(),
                    "Execute cargo test -p codex-tui".to_string(),
                    "Fetch Ticket COD-123".to_string(),
                    "Index Symbols crate:codex-tui".to_string(),
                ],
            }
        );
    }
}
