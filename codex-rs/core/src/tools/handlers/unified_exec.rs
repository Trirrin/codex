use crate::function_tool::FunctionCallError;
use crate::maybe_emit_implicit_skill_invocation;
use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::get_shell_by_model_provided_path;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::implicit_granted_permissions;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_workdir_base_path;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::unified_exec::ActiveShell;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::ShellOutputRequest;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::resolve_max_tokens;
use codex_features::Feature;
use codex_otel::SessionTelemetry;
use codex_otel::TOOL_CALL_UNIFIED_EXEC_METRIC;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandRunMode;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_shell_command::is_safe_command::is_known_safe_command;
use codex_tools::UnifiedExecShellMode;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

pub struct UnifiedExecHandler;

#[derive(Debug, Deserialize)]
pub(crate) struct ExecCommandArgs {
    cmd: String,
    #[serde(default)]
    mode: Option<ExecuteMode>,
    #[serde(default)]
    pub(crate) workdir: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    login: Option<bool>,
    #[serde(default = "default_tty")]
    tty: bool,
    #[serde(default = "default_write_stdin_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    sandbox_permissions: SandboxPermissions,
    #[serde(default)]
    additional_permissions: Option<AdditionalPermissionProfile>,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default)]
    prefix_rule: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct WriteStdinArgs {
    #[serde(alias = "session_id")]
    process_id: i32,
    chars: String,
    #[serde(default = "default_exec_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ShellOutputArgs {
    shell_id: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct StopShellArgs {
    shell_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum ExecuteMode {
    #[default]
    Blocking,
    Background,
}

fn default_exec_yield_time_ms() -> u64 {
    10_000
}

fn default_write_stdin_yield_time_ms() -> u64 {
    250
}

fn default_tty() -> bool {
    false
}

fn effective_max_output_tokens(
    max_output_tokens: Option<usize>,
    truncation_policy: TruncationPolicy,
) -> usize {
    resolve_max_tokens(max_output_tokens).min(truncation_policy.token_budget())
}

impl ToolHandler for UnifiedExecHandler {
    type Output = ExecCommandToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            tracing::error!(
                "This should never happen, invocation payload is wrong: {:?}",
                invocation.payload
            );
            return true;
        };

        if matches!(
            invocation.tool_name.name.as_str(),
            "read_shell_output" | "wait_shell_output" | "list_shells"
        ) {
            return false;
        }

        if invocation.tool_name.name.as_str() == "stop_shell" {
            return true;
        }

        let Ok(params) = parse_arguments::<ExecCommandArgs>(arguments) else {
            return true;
        };
        if should_block_plain_inspection_command(&params.cmd) {
            return false;
        }
        let command = match get_command(
            &params,
            invocation.session.user_shell(),
            &invocation.turn.tools_config.unified_exec_shell_mode,
            invocation.turn.tools_config.allow_login_shell,
        ) {
            Ok(command) => command,
            Err(_) => return true,
        };
        !is_known_safe_command(&command)
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let name = invocation.tool_name.name.as_str();
        if invocation.tool_name.namespace.is_some() || !matches!(name, "execute" | "exec_command") {
            return None;
        }

        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        parse_arguments::<ExecCommandArgs>(arguments)
            .ok()
            .filter(|args| !should_block_plain_inspection_command(&args.cmd))
            .map(|args| PreToolUsePayload {
                tool_name: HookToolName::bash(),
                tool_input: serde_json::json!({ "command": args.cmd }),
            })
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &Self::Output,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { .. } = &invocation.payload else {
            return None;
        };
        if matches!(
            invocation.tool_name.name.as_str(),
            "read_shell_output" | "wait_shell_output" | "list_shells" | "stop_shell"
        ) {
            return None;
        }

        let command = result.hook_command.clone()?;
        let tool_use_id = if result.event_call_id.is_empty() {
            invocation.call_id.clone()
        } else {
            result.event_call_id.clone()
        };
        let tool_response = result.post_tool_use_response(&tool_use_id, &invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_use_id,
            tool_input: serde_json::json!({ "command": command }),
            tool_response,
        })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "unified_exec handler received unsupported payload".to_string(),
                ));
            }
        };

        let Some(environment) = turn.environment.as_ref() else {
            return Err(FunctionCallError::RespondToModel(
                "unified exec is unavailable in this session".to_string(),
            ));
        };
        let fs = environment.get_filesystem();

        let manager: &UnifiedExecProcessManager = &session.services.unified_exec_manager;
        let context = UnifiedExecContext::new(session.clone(), turn.clone(), call_id.clone());

        let response = match tool_name.name.as_str() {
            "execute" | "exec_command" => {
                let legacy_exec_command = tool_name.name.as_str() == "exec_command";
                let cwd = resolve_workdir_base_path(&arguments, &context.turn.cwd)?;
                let args: ExecCommandArgs = parse_arguments_with_base_path(&arguments, &cwd)?;
                let hook_command = args.cmd.clone();
                if should_block_plain_inspection_command(&args.cmd) {
                    return Ok(blocked_existing_tools_output(args.max_output_tokens));
                }
                let workdir = context.turn.resolve_path(args.workdir.clone());
                maybe_emit_implicit_skill_invocation(
                    session.as_ref(),
                    context.turn.as_ref(),
                    &hook_command,
                    &workdir,
                )
                .await;
                let process_id = manager.allocate_process_id().await;
                let command = get_command(
                    &args,
                    session.user_shell(),
                    &turn.tools_config.unified_exec_shell_mode,
                    turn.tools_config.allow_login_shell,
                )
                .map_err(FunctionCallError::RespondToModel)?;
                let command_for_display = codex_shell_command::parse_command::shlex_join(&command);

                let ExecCommandArgs {
                    mode,
                    workdir,
                    tty,
                    yield_time_ms,
                    max_output_tokens,
                    sandbox_permissions,
                    additional_permissions,
                    justification,
                    prefix_rule,
                    ..
                } = args;
                let mode = mode.unwrap_or(if legacy_exec_command {
                    ExecuteMode::Background
                } else {
                    ExecuteMode::Blocking
                });
                let run_mode = match mode {
                    ExecuteMode::Blocking => ExecCommandRunMode::Blocking,
                    ExecuteMode::Background => ExecCommandRunMode::Background,
                };
                let max_output_tokens =
                    effective_max_output_tokens(max_output_tokens, turn.truncation_policy);

                let exec_permission_approvals_enabled =
                    session.features().enabled(Feature::ExecPermissionApprovals);
                let requested_additional_permissions = additional_permissions.clone();
                let effective_additional_permissions = apply_granted_turn_permissions(
                    context.session.as_ref(),
                    context.turn.cwd.as_path(),
                    sandbox_permissions,
                    additional_permissions,
                )
                .await;
                let additional_permissions_allowed = exec_permission_approvals_enabled
                    || (session.features().enabled(Feature::RequestPermissionsTool)
                        && effective_additional_permissions.permissions_preapproved);

                let workdir = workdir.filter(|value| !value.is_empty());

                let workdir = workdir.map(|dir| context.turn.resolve_path(Some(dir)));
                let cwd = workdir.clone().unwrap_or(cwd);
                let normalized_additional_permissions = match implicit_granted_permissions(
                    sandbox_permissions,
                    requested_additional_permissions.as_ref(),
                    &effective_additional_permissions,
                )
                .map_or_else(
                    || {
                        normalize_and_validate_additional_permissions(
                            additional_permissions_allowed,
                            context.turn.approval_policy.value(),
                            effective_additional_permissions.sandbox_permissions,
                            effective_additional_permissions.additional_permissions,
                            effective_additional_permissions.permissions_preapproved,
                            &cwd,
                        )
                    },
                    |permissions| Ok(Some(permissions)),
                ) {
                    Ok(normalized) => normalized,
                    Err(err) => {
                        manager.release_process_id(process_id).await;
                        return Err(FunctionCallError::RespondToModel(err));
                    }
                };

                if let Some(output) = intercept_apply_patch(
                    &command,
                    &cwd,
                    fs.as_ref(),
                    context.session.clone(),
                    context.turn.clone(),
                    Some(&tracker),
                    &context.call_id,
                    &tool_name.name,
                )
                .await?
                {
                    manager.release_process_id(process_id).await;
                    return Ok(ExecCommandToolOutput {
                        event_call_id: String::new(),
                        chunk_id: String::new(),
                        wall_time: std::time::Duration::ZERO,
                        raw_output: output.into_text().into_bytes(),
                        max_output_tokens: Some(max_output_tokens),
                        shell_id: None,
                        process_id: None,
                        exit_code: None,
                        original_token_count: None,
                        hook_command: None,
                    });
                }

                emit_unified_exec_tty_metric(&turn.session_telemetry, tty);
                let request = ExecCommandRequest {
                    command,
                    hook_command: hook_command.clone(),
                    process_id,
                    run_mode,
                    yield_time_ms,
                    max_output_tokens: Some(max_output_tokens),
                    workdir,
                    network: context.turn.network.clone(),
                    tty,
                    sandbox_permissions: effective_additional_permissions.sandbox_permissions,
                    additional_permissions: normalized_additional_permissions,
                    additional_permissions_preapproved: effective_additional_permissions
                        .permissions_preapproved,
                    justification,
                    exec_approval_requirement_override: None,
                    prefix_rule,
                };
                execute_with_blocking_wait(
                    manager,
                    &context,
                    request,
                    mode,
                    max_output_tokens,
                    &command_for_display,
                )
                .await?
            }
            "read_shell_output" => {
                let args: ShellOutputArgs = parse_arguments(&arguments)?;
                let process_id = parse_shell_id(&args.shell_id)?;
                let max_output_tokens =
                    effective_max_output_tokens(args.max_output_tokens, turn.truncation_policy);
                let emitter = shell_output_tool_emitter(
                    "read_shell_output",
                    &args.shell_id,
                    &context.turn.cwd,
                );
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter.emit(event_ctx, ToolEventStage::Begin).await;
                let response = match manager
                    .read_shell_output(ShellOutputRequest {
                        process_id,
                        yield_time_ms: None,
                        max_output_tokens: Some(max_output_tokens),
                    })
                    .await
                {
                    Ok(response) => response,
                    Err(err) => {
                        let message = format!(
                            "read_shell_output failed for shell {}: {err}",
                            args.shell_id
                        );
                        let event_ctx = ToolEventCtx::new(
                            context.session.as_ref(),
                            context.turn.as_ref(),
                            &context.call_id,
                            /*turn_diff_tracker*/ None,
                        );
                        emitter
                            .emit(
                                event_ctx,
                                ToolEventStage::Failure(ToolEventFailure::Message(message.clone())),
                            )
                            .await;
                        return Err(FunctionCallError::RespondToModel(message));
                    }
                };
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter
                    .emit(
                        event_ctx,
                        ToolEventStage::Success(exec_output_from_response(&response)),
                    )
                    .await;
                response
            }
            "wait_shell_output" => {
                let args: ShellOutputArgs = parse_arguments(&arguments)?;
                let process_id = parse_shell_id(&args.shell_id)?;
                let max_output_tokens =
                    effective_max_output_tokens(args.max_output_tokens, turn.truncation_policy);
                let emitter = shell_output_tool_emitter(
                    "wait_shell_output",
                    &args.shell_id,
                    &context.turn.cwd,
                );
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter.emit(event_ctx, ToolEventStage::Begin).await;
                let response = match manager
                    .wait_shell_output(ShellOutputRequest {
                        process_id,
                        yield_time_ms: args.yield_time_ms,
                        max_output_tokens: Some(max_output_tokens),
                    })
                    .await
                {
                    Ok(response) => response,
                    Err(err) => {
                        let message = format!(
                            "wait_shell_output failed for shell {}: {err}",
                            args.shell_id
                        );
                        let event_ctx = ToolEventCtx::new(
                            context.session.as_ref(),
                            context.turn.as_ref(),
                            &context.call_id,
                            /*turn_diff_tracker*/ None,
                        );
                        emitter
                            .emit(
                                event_ctx,
                                ToolEventStage::Failure(ToolEventFailure::Message(message.clone())),
                            )
                            .await;
                        return Err(FunctionCallError::RespondToModel(message));
                    }
                };
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter
                    .emit(
                        event_ctx,
                        ToolEventStage::Success(exec_output_from_response(&response)),
                    )
                    .await;
                response
            }
            "list_shells" => {
                let emitter = shell_management_tool_emitter("list_shells", None, &context.turn.cwd);
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter.emit(event_ctx, ToolEventStage::Begin).await;
                let response = shell_list_response(manager.active_shells().await);
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter
                    .emit(
                        event_ctx,
                        ToolEventStage::Success(exec_output_from_response(&response)),
                    )
                    .await;
                response
            }
            "stop_shell" => {
                let args: StopShellArgs = parse_arguments(&arguments)?;
                let process_id = parse_shell_id(&args.shell_id)?;
                let emitter = shell_management_tool_emitter(
                    "stop_shell",
                    Some(&args.shell_id),
                    &context.turn.cwd,
                );
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter.emit(event_ctx, ToolEventStage::Begin).await;
                let stopped = match manager.terminate_process(process_id).await {
                    Ok(stopped) => stopped,
                    Err(err) => {
                        let message =
                            format!("stop_shell failed for shell {}: {err}", args.shell_id);
                        let event_ctx = ToolEventCtx::new(
                            context.session.as_ref(),
                            context.turn.as_ref(),
                            &context.call_id,
                            /*turn_diff_tracker*/ None,
                        );
                        emitter
                            .emit(
                                event_ctx,
                                ToolEventStage::Failure(ToolEventFailure::Message(message.clone())),
                            )
                            .await;
                        return Err(FunctionCallError::RespondToModel(message));
                    }
                };
                let response = shell_stop_response(stopped);
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    /*turn_diff_tracker*/ None,
                );
                emitter
                    .emit(
                        event_ctx,
                        ToolEventStage::Success(exec_output_from_response(&response)),
                    )
                    .await;
                response
            }
            "write_stdin" => {
                let args: WriteStdinArgs = parse_arguments(&arguments)?;
                let max_output_tokens =
                    effective_max_output_tokens(args.max_output_tokens, turn.truncation_policy);
                let response = manager
                    .write_stdin(WriteStdinRequest {
                        process_id: args.process_id,
                        input: &args.chars,
                        yield_time_ms: args.yield_time_ms,
                        max_output_tokens: Some(max_output_tokens),
                    })
                    .await
                    .map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "write_stdin failed for process {}: {err}",
                            args.process_id
                        ))
                    })?;
                context
                    .session
                    .send_event(
                        context.turn.as_ref(),
                        EventMsg::TerminalInteraction(TerminalInteractionEvent {
                            call_id: response.event_call_id.clone(),
                            process_id: args.process_id.to_string(),
                            stdin: args.chars,
                        }),
                    )
                    .await;
                response
            }
            other => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unsupported unified exec function {other}"
                )));
            }
        };

        Ok(response)
    }
}

const USE_EXISTING_TOOLS_MESSAGE: &str = "Use the existing file tools instead of plain rg, grep, or cat for file inspection. Do not respond to this tool result; continue with the user's task using the appropriate existing tool.";

fn should_block_plain_inspection_command(command: &str) -> bool {
    let Some(tokens) = shlex::split(command) else {
        return false;
    };
    let Some(command) = primary_command_token(&tokens) else {
        return false;
    };
    let command = command.rsplit(['/', '\\']).next().unwrap_or(command);
    matches!(command, "cat" | "grep" | "rg")
}

fn primary_command_token(tokens: &[String]) -> Option<&str> {
    for token in tokens.iter().map(String::as_str) {
        if token == "|" {
            return None;
        }
        if token.contains('=') && !token.starts_with('/') {
            continue;
        }
        if matches!(token, "command" | "env" | "sudo" | "time") || token.starts_with('-') {
            continue;
        }
        return Some(token);
    }
    None
}

fn blocked_existing_tools_output(max_output_tokens: Option<usize>) -> ExecCommandToolOutput {
    ExecCommandToolOutput {
        event_call_id: String::new(),
        chunk_id: String::new(),
        wall_time: std::time::Duration::ZERO,
        raw_output: USE_EXISTING_TOOLS_MESSAGE.as_bytes().to_vec(),
        max_output_tokens,
        shell_id: None,
        process_id: None,
        exit_code: None,
        original_token_count: None,
        hook_command: None,
    }
}

fn shell_output_tool_emitter(
    tool_name: &str,
    shell_id: &str,
    cwd: &codex_utils_absolute_path::AbsolutePathBuf,
) -> ToolEmitter {
    ToolEmitter::unified_exec(
        &[
            "bash".to_string(),
            "-lc".to_string(),
            format!("{tool_name} {shell_id}"),
        ],
        cwd.clone(),
        ExecCommandSource::UnifiedExecInteraction,
        Some(shell_id.to_string()),
        None,
    )
}

fn shell_management_tool_emitter(
    tool_name: &str,
    shell_id: Option<&str>,
    cwd: &codex_utils_absolute_path::AbsolutePathBuf,
) -> ToolEmitter {
    let command = shell_id.map_or_else(
        || tool_name.to_string(),
        |shell_id| format!("{tool_name} {shell_id}"),
    );
    ToolEmitter::unified_exec(
        &["bash".to_string(), "-lc".to_string(), command],
        cwd.clone(),
        ExecCommandSource::UnifiedExecInteraction,
        shell_id.map(str::to_string),
        None,
    )
}

fn shell_list_response(shells: Vec<ActiveShell>) -> ExecCommandToolOutput {
    let output = if shells.is_empty() {
        "No active shells.".to_string()
    } else {
        let mut lines = vec![format!("{} active shell(s):", shells.len())];
        lines.extend(shells.into_iter().map(|shell| {
            format!(
                "- shell_id: {}, runtime: {}, command: {}",
                shell.shell_id,
                format_duration(shell.runtime),
                shell.command
            )
        }));
        lines.join("\n")
    };
    text_tool_output(output, None, None)
}

fn shell_stop_response(shell: ActiveShell) -> ExecCommandToolOutput {
    text_tool_output(
        format!(
            "Stopped shell {} after {}: {}",
            shell.shell_id,
            format_duration(shell.runtime),
            shell.command
        ),
        Some(shell.shell_id.to_string()),
        None,
    )
}

fn text_tool_output(
    output: String,
    shell_id: Option<String>,
    process_id: Option<i32>,
) -> ExecCommandToolOutput {
    let original_token_count = approx_token_count(&output);
    ExecCommandToolOutput {
        event_call_id: String::new(),
        chunk_id: generate_chunk_id(),
        wall_time: std::time::Duration::ZERO,
        raw_output: output.into_bytes(),
        max_output_tokens: None,
        shell_id,
        process_id,
        exit_code: Some(0),
        original_token_count: Some(original_token_count),
        hook_command: None,
    }
}

fn format_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

async fn execute_with_blocking_wait(
    manager: &UnifiedExecProcessManager,
    context: &UnifiedExecContext,
    mut request: ExecCommandRequest,
    mode: ExecuteMode,
    max_output_tokens: usize,
    command_for_display: &str,
) -> Result<ExecCommandToolOutput, FunctionCallError> {
    loop {
        let response = manager.exec_command(request.clone(), context).await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "execute failed for `{command_for_display}`: {err:?}"
                )));
            }
        };

        match mode {
            ExecuteMode::Background => return Ok(response),
            ExecuteMode::Blocking => {
                match wait_for_process_exit(manager, response, max_output_tokens).await {
                    Ok(response) => return Ok(response),
                    Err(WaitForProcessExitError::SandboxDenied) => {
                        escalate_request_for_retry(manager, &mut request, "execute blocking wait")
                            .await
                            .map_err(FunctionCallError::RespondToModel)?;
                    }
                    Err(WaitForProcessExitError::Message(message)) => {
                        return Err(FunctionCallError::RespondToModel(message));
                    }
                }
            }
        }
    }
}

async fn escalate_request_for_retry(
    manager: &UnifiedExecProcessManager,
    request: &mut ExecCommandRequest,
    operation: &str,
) -> Result<(), String> {
    if matches!(
        request.sandbox_permissions,
        SandboxPermissions::RequireEscalated
    ) {
        return Err(format!("{operation} failed: command denied by sandbox"));
    }

    request.exec_approval_requirement_override = Some(ExecApprovalRequirement::NeedsApproval {
        reason: Some("command failed; retry without sandbox?".to_string()),
        proposed_execpolicy_amendment: None,
    });
    request.process_id = manager.allocate_process_id().await;
    request.sandbox_permissions = SandboxPermissions::RequireEscalated;
    request.additional_permissions = None;
    request.additional_permissions_preapproved = false;
    Ok(())
}

fn exec_output_from_response(response: &ExecCommandToolOutput) -> ExecToolCallOutput {
    let output = String::from_utf8_lossy(&response.raw_output).to_string();
    ExecToolCallOutput {
        exit_code: response.exit_code.unwrap_or(0),
        stdout: StreamOutput::new(output.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(output),
        duration: response.wall_time,
        timed_out: false,
    }
}

fn parse_shell_id(shell_id: &str) -> Result<i32, FunctionCallError> {
    shell_id.parse::<i32>().map_err(|_| {
        FunctionCallError::RespondToModel(format!(
            "invalid shell_id `{shell_id}`: expected the id returned by execute mode=\"background\""
        ))
    })
}

fn emit_unified_exec_tty_metric(session_telemetry: &SessionTelemetry, tty: bool) {
    session_telemetry.counter(
        TOOL_CALL_UNIFIED_EXEC_METRIC,
        /*inc*/ 1,
        &[("tty", if tty { "true" } else { "false" })],
    );
}

#[derive(Debug)]
enum WaitForProcessExitError {
    Message(String),
    SandboxDenied,
}

async fn wait_for_process_exit(
    manager: &UnifiedExecProcessManager,
    mut response: ExecCommandToolOutput,
    max_output_tokens: usize,
) -> Result<ExecCommandToolOutput, WaitForProcessExitError> {
    let Some(mut process_id) = response.process_id else {
        return Ok(response);
    };

    loop {
        let next = manager
            .write_stdin(WriteStdinRequest {
                process_id,
                input: "",
                yield_time_ms: default_exec_yield_time_ms(),
                max_output_tokens: Some(max_output_tokens),
            })
            .await;
        let (next, full_snapshot) = match next {
            Ok(next) => (next, false),
            Err(UnifiedExecError::UnknownProcessId {
                process_id: err_process_id,
            }) if err_process_id == process_id => {
                let snapshot = manager
                    .read_shell_output(ShellOutputRequest {
                        process_id,
                        yield_time_ms: None,
                        max_output_tokens: Some(max_output_tokens),
                    })
                    .await
                    .map_err(|err| {
                        WaitForProcessExitError::Message(format!(
                            "execute blocking wait failed: {err}"
                        ))
                    })?;
                (snapshot, true)
            }
            Err(UnifiedExecError::SandboxDenied { .. }) => {
                return Err(WaitForProcessExitError::SandboxDenied);
            }
            Err(err) => {
                return Err(WaitForProcessExitError::Message(format!(
                    "execute blocking wait failed: {err}"
                )));
            }
        };
        if full_snapshot {
            response.raw_output = next.raw_output;
            response.wall_time = next.wall_time;
            response.original_token_count = next.original_token_count;
        } else {
            response.raw_output.extend(next.raw_output);
            response.wall_time += next.wall_time;
            response.original_token_count = response
                .original_token_count
                .zip(next.original_token_count)
                .map(|(left, right)| left + right)
                .or(response.original_token_count)
                .or(next.original_token_count);
        }
        response.exit_code = next.exit_code;
        response.shell_id = next.shell_id.clone();
        response.process_id = next.process_id;
        if let Some(next_process_id) = next.process_id {
            process_id = next_process_id;
        } else {
            return Ok(response);
        }
    }
}

pub(crate) fn get_command(
    args: &ExecCommandArgs,
    session_shell: Arc<Shell>,
    shell_mode: &UnifiedExecShellMode,
    allow_login_shell: bool,
) -> Result<Vec<String>, String> {
    let use_login_shell = match args.login {
        Some(true) if !allow_login_shell => {
            return Err(
                "login shell is disabled by config; omit `login` or set it to false.".to_string(),
            );
        }
        Some(use_login_shell) => use_login_shell,
        None => allow_login_shell,
    };

    match shell_mode {
        UnifiedExecShellMode::Direct => {
            let model_shell = args.shell.as_ref().map(|shell_str| {
                let mut shell = get_shell_by_model_provided_path(&PathBuf::from(shell_str));
                shell.shell_snapshot = crate::shell::empty_shell_snapshot_receiver();
                shell
            });
            let shell = model_shell.as_ref().unwrap_or(session_shell.as_ref());
            Ok(shell.derive_exec_args(&args.cmd, use_login_shell))
        }
        UnifiedExecShellMode::ZshFork(zsh_fork_config) => Ok(vec![
            zsh_fork_config.shell_zsh_path.to_string_lossy().to_string(),
            if use_login_shell { "-lc" } else { "-c" }.to_string(),
            args.cmd.clone(),
        ]),
    }
}

#[cfg(test)]
#[path = "unified_exec_tests.rs"]
mod tests;
