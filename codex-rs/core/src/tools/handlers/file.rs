use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::permissions::ReadDenyMatcher;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::PatchApplyEndEvent;
use codex_protocol::protocol::PatchApplyStatus;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text;
use regex_lite::RegexBuilder;
use serde::Deserialize;
use serde::Serialize;
use similar::ChangeTag;
use similar::TextDiff;
use std::collections::HashMap;
use tokio::fs;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::turn_diff_tracker::FileObservationError;
use crate::unified_exec::resolve_max_tokens;

pub struct FileHandler;

const DEFAULT_LIMIT: usize = 100;
const DEFAULT_SEARCH_LIMIT: usize = 50;
const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
];

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    include_line_numbers: bool,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct SearchFileArgs {
    query: String,
    #[serde(default)]
    roots: Vec<String>,
    #[serde(default = "default_search_limit")]
    limit: usize,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default)]
    exclude_globs: Vec<String>,
    #[serde(default)]
    output_format: OutputFormat,
}

#[derive(Deserialize)]
struct GrepFileArgs {
    pattern: String,
    #[serde(default)]
    roots: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default)]
    exclude_globs: Vec<String>,
    #[serde(default = "default_true")]
    case_sensitive: bool,
    #[serde(default = "default_search_limit")]
    limit: usize,
    #[serde(default)]
    context: usize,
    #[serde(default)]
    output_format: OutputFormat,
}

#[derive(Deserialize)]
struct GlobFileArgs {
    pattern: String,
    #[serde(default)]
    root: Option<String>,
    #[serde(default = "default_search_limit")]
    limit: usize,
    #[serde(default)]
    relative_only: bool,
    #[serde(default)]
    entry_type: GlobEntryType,
    #[serde(default)]
    output_format: OutputFormat,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum GlobEntryType {
    #[default]
    File,
    Dir,
    Any,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    #[serde(default)]
    apply_mode: EditApplyMode,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    occurrence: Option<usize>,
    #[serde(default)]
    ops: Vec<EditOperation>,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum EditApplyMode {
    #[default]
    Sequential,
    Snapshot,
}

struct EditRequest {
    apply_mode: EditApplyMode,
    operations: Vec<EditOperation>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EditOperation {
    Replace {
        old_text: String,
        new_text: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    InsertBefore {
        anchor: String,
        text: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    InsertAfter {
        anchor: String,
        text: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    ReplaceRange {
        start_line: usize,
        end_line: usize,
        new_text: String,
    },
    DeleteRange {
        start_line: usize,
        end_line: usize,
    },
    DeleteText {
        old_text: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    ReplaceOccurrences {
        old_text: String,
        new_text: String,
        occurrences: Vec<usize>,
    },
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    create_parent_dirs: bool,
    #[serde(default)]
    mode: WriteMode,
    #[serde(default)]
    expected_hash: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum WriteMode {
    #[default]
    Overwrite,
    CreateNew,
    UpdateExisting,
}

#[derive(Deserialize)]
struct DeleteArgs {
    path: String,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    expected_type: Option<DeleteExpectedType>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum DeleteExpectedType {
    File,
    Dir,
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

fn default_search_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

fn default_true() -> bool {
    true
}

fn read_file_display_command(args: &ReadFileArgs) -> String {
    let mut parts = vec!["read_file".to_string()];
    if let Some(start_line) = args.start_line {
        parts.push(format!("--start-line={start_line}"));
    } else if args.offset != 0 {
        let offset = args.offset;
        parts.push(format!("--offset={offset}"));
    }
    if let Some(end_line) = args.end_line {
        parts.push(format!("--end-line={end_line}"));
    } else if args.start_line.is_some() || args.offset != 0 || args.limit != DEFAULT_LIMIT {
        let limit = args.limit;
        parts.push(format!("--limit={limit}"));
    }
    parts.join(" ")
}

#[derive(Serialize)]
struct SearchMatch {
    path: String,
    relative_path: Option<String>,
    score: i32,
}

#[derive(Serialize)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
    context_before: Vec<GrepContextLine>,
    context_after: Vec<GrepContextLine>,
}

#[derive(Serialize)]
struct GrepContextLine {
    line: usize,
    text: String,
}

struct FileLineObservation {
    path: PathBuf,
    content_hash: String,
    lines: Vec<usize>,
}

#[derive(Serialize)]
struct GlobMatch {
    path: String,
    relative_path: Option<String>,
    entry_type: &'static str,
}

#[derive(Clone, Copy)]
struct PathFilters<'a> {
    include: &'a [String],
    exclude: &'a [String],
    include_globs: &'a [String],
    exclude_globs: &'a [String],
}

struct GrepCollectOptions<'a> {
    regex: &'a regex_lite::Regex,
    filters: PathFilters<'a>,
    context: usize,
    limit: usize,
}

impl ToolHandler for FileHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        matches!(
            invocation.tool_name.name.as_str(),
            "edit" | "write" | "delete"
        )
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            payload,
            turn,
            tracker,
            tool_name,
            call_id,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "file handler received unsupported payload".to_string(),
            ));
        };

        match tool_name.name.as_str() {
            "read_file" => {
                let args: ReadFileArgs = parse_arguments(&arguments)?;
                let path = resolve_path(turn.cwd.as_path(), &args.path);
                let parsed = vec![ParsedCommand::Read {
                    cmd: read_file_display_command(&args),
                    name: args.path.clone(),
                    path: path.clone(),
                }];
                let started = emit_tool_begin(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["read_file".to_string(), args.path.clone()],
                    parsed.clone(),
                )
                .await;
                let result = async {
                    ensure_read_allowed(turn.as_ref(), &path)?;
                    let text = fs::read_to_string(&path).await.map_err(io_error)?;
                    let lines = read_lines(&text, &args)?;
                    let observed_ranges = numbered_line_ranges(&lines);
                    let output = format_read_lines(&lines, args.include_line_numbers);
                    let output = truncate(output, args.max_output_tokens);
                    record_observed_ranges(&tracker, &path, &text, observed_ranges).await;
                    Ok(FunctionToolOutput::from_text(output, Some(true)))
                }
                .await;
                emit_tool_end(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["read_file".to_string(), args.path],
                    parsed,
                    started,
                    &result,
                )
                .await;
                result
            }
            "search_file" => {
                let args: SearchFileArgs = parse_arguments(&arguments)?;
                let parsed = vec![ParsedCommand::Search {
                    cmd: "search_file".to_string(),
                    query: Some(args.query.clone()),
                    path: args.roots.first().cloned(),
                }];
                let started = emit_tool_begin(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["search_file".to_string(), args.query.clone()],
                    parsed.clone(),
                )
                .await;
                let roots = roots_or_cwd(turn.cwd.as_path(), args.roots);
                let result = async {
                    let mut matches = Vec::new();
                    let filters = PathFilters {
                        include: &args.include,
                        exclude: &args.exclude,
                        include_globs: &args.include_globs,
                        exclude_globs: &args.exclude_globs,
                    };
                    for root in &roots {
                        collect_path_matches(
                            turn.as_ref(),
                            root,
                            &args.query,
                            filters,
                            args.limit,
                            &mut matches,
                        )
                        .await?;
                        if matches.len() >= args.limit {
                            break;
                        }
                    }
                    matches.sort_by_key(|entry| entry.score);
                    matches.truncate(args.limit);
                    let warning = glob_filter_warning(&args.include, &args.exclude);
                    let output = match args.output_format {
                        OutputFormat::Text => text_output_with_warnings(
                            warning,
                            matches
                                .iter()
                                .map(|entry| entry.path.as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        OutputFormat::Json => serde_json::to_string_pretty(&matches)
                            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?,
                    };
                    Ok(FunctionToolOutput::from_text(output, Some(true)))
                }
                .await;
                emit_tool_end(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["search_file".to_string(), args.query],
                    parsed,
                    started,
                    &result,
                )
                .await;
                result
            }
            "grep_file" => {
                let args: GrepFileArgs = parse_arguments(&arguments)?;
                let parsed = vec![ParsedCommand::Search {
                    cmd: "grep_file".to_string(),
                    query: Some(args.pattern.clone()),
                    path: args.roots.first().cloned(),
                }];
                let started = emit_tool_begin(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["grep_file".to_string(), args.pattern.clone()],
                    parsed.clone(),
                )
                .await;
                let roots = roots_or_cwd(turn.cwd.as_path(), args.roots);
                let result = async {
                    let regex = RegexBuilder::new(&args.pattern)
                        .case_insensitive(!args.case_sensitive)
                        .build()
                        .map_err(|err| {
                            FunctionCallError::RespondToModel(format!("invalid regex: {err}"))
                        })?;
                    let warning = glob_filter_warning(&args.include, &args.exclude);
                    let mut observations = Vec::new();
                    let output = match args.output_format {
                        OutputFormat::Text => {
                            let mut matches = Vec::new();
                            let options = GrepCollectOptions {
                                regex: &regex,
                                filters: PathFilters {
                                    include: &args.include,
                                    exclude: &args.exclude,
                                    include_globs: &args.include_globs,
                                    exclude_globs: &args.exclude_globs,
                                },
                                context: args.context,
                                limit: args.limit,
                            };
                            for root in &roots {
                                collect_grep_text_matches(
                                    turn.as_ref(),
                                    root,
                                    &options,
                                    &mut matches,
                                    &mut observations,
                                )
                                .await?;
                                if matches.len() >= args.limit {
                                    break;
                                }
                            }
                            text_output_with_warnings(warning, matches.join("\n"))
                        }
                        OutputFormat::Json => {
                            let mut matches = Vec::new();
                            let options = GrepCollectOptions {
                                regex: &regex,
                                filters: PathFilters {
                                    include: &args.include,
                                    exclude: &args.exclude,
                                    include_globs: &args.include_globs,
                                    exclude_globs: &args.exclude_globs,
                                },
                                context: args.context,
                                limit: args.limit,
                            };
                            for root in &roots {
                                collect_grep_structured_matches(
                                    turn.as_ref(),
                                    root,
                                    &options,
                                    &mut matches,
                                    &mut observations,
                                )
                                .await?;
                                if matches.len() >= args.limit {
                                    break;
                                }
                            }
                            serde_json::to_string_pretty(&matches)
                                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?
                        }
                    };
                    record_file_line_observations(&tracker, observations).await;
                    Ok(FunctionToolOutput::from_text(output, Some(true)))
                }
                .await;
                emit_tool_end(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["grep_file".to_string(), args.pattern],
                    parsed,
                    started,
                    &result,
                )
                .await;
                result
            }
            "glob_file" => {
                let args: GlobFileArgs = parse_arguments(&arguments)?;
                let parsed = vec![ParsedCommand::Search {
                    cmd: "glob_file".to_string(),
                    query: Some(args.pattern.clone()),
                    path: args.root.clone(),
                }];
                let started = emit_tool_begin(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["glob_file".to_string(), args.pattern.clone()],
                    parsed.clone(),
                )
                .await;
                let root = args.root.map_or_else(
                    || turn.cwd.as_path().to_path_buf(),
                    |root| resolve_path(turn.cwd.as_path(), &root),
                );
                let result = async {
                    let mut matches = Vec::new();
                    collect_glob_matches(
                        turn.as_ref(),
                        &root,
                        &args.pattern,
                        args.entry_type,
                        args.limit,
                        &mut matches,
                    )
                    .await?;
                    let output = match args.output_format {
                        OutputFormat::Text => matches
                            .iter()
                            .map(|entry| {
                                if args.relative_only {
                                    entry
                                        .relative_path
                                        .as_deref()
                                        .unwrap_or(entry.path.as_str())
                                } else {
                                    entry.path.as_str()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        OutputFormat::Json => serde_json::to_string_pretty(&matches)
                            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?,
                    };
                    Ok(FunctionToolOutput::from_text(output, Some(true)))
                }
                .await;
                emit_tool_end(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    vec!["glob_file".to_string(), args.pattern],
                    parsed,
                    started,
                    &result,
                )
                .await;
                result
            }
            "edit" => {
                let args: EditArgs = parse_arguments(&arguments)?;
                let path = resolve_path(turn.cwd.as_path(), &args.path);
                ensure_write_allowed(turn.as_ref(), &path).await?;
                let request = edit_request(args)?;
                let warning = sequential_edit_warning(&request.operations, request.apply_mode);
                let text = fs::read_to_string(&path).await.map_err(io_error)?;
                let observation_plan = edit_observation_plan(&text, &request)?;
                let previous_hash = content_hash(&text);
                ensure_ranges_observed_for_edit(
                    &tracker,
                    &path,
                    &previous_hash,
                    &observation_plan.required_ranges,
                )
                .await?;
                let applied = apply_edit_operations(&text, &request)?;
                let edited = applied.text;
                let diff = edit_output(&path, &text, &edited, warning, &applied.hits);
                let changed_ranges = changed_line_ranges(&text, &edited);
                let changes = single_update_change(&path, &text, &edited);
                emit_patch_begin(session.as_ref(), turn.as_ref(), &call_id, &changes).await;
                let result = fs::write(&path, &edited)
                    .await
                    .map_err(io_error)
                    .map(|_| FunctionToolOutput::from_text(diff, Some(true)));
                if result.is_ok() {
                    record_edit_observation(
                        &tracker,
                        &path,
                        &previous_hash,
                        &edited,
                        &observation_plan.final_line_origins,
                        changed_ranges,
                    )
                    .await;
                }
                emit_patch_end(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    changes,
                    &result,
                    "edited",
                )
                .await;
                result
            }
            "write" => {
                let args: WriteArgs = parse_arguments(&arguments)?;
                let path = resolve_path(turn.cwd.as_path(), &args.path);
                ensure_write_allowed(turn.as_ref(), &path).await?;
                if args.create_parent_dirs
                    && let Some(parent) = path.parent()
                {
                    fs::create_dir_all(parent).await.map_err(io_error)?;
                    ensure_write_allowed(turn.as_ref(), &path).await?;
                }
                let exists = fs::try_exists(&path).await.map_err(io_error)?;
                match args.mode {
                    WriteMode::CreateNew if exists => {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "refusing to overwrite existing file `{}` with mode=create_new",
                            path.display()
                        )));
                    }
                    WriteMode::UpdateExisting if !exists => {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "refusing to create missing file `{}` with mode=update_existing",
                            path.display()
                        )));
                    }
                    WriteMode::Overwrite | WriteMode::CreateNew | WriteMode::UpdateExisting => {}
                }
                let previous = fs::read_to_string(&path).await.ok();
                if let Some(expected_hash) = args.expected_hash.as_deref() {
                    let Some(previous) = previous.as_deref() else {
                        return Err(FunctionCallError::RespondToModel(
                            "expected_hash requires an existing readable text file".to_string(),
                        ));
                    };
                    ensure_expected_hash(previous, expected_hash)?;
                }
                let content = args.content;
                let changes = single_write_change(&path, previous.as_deref(), &content);
                emit_patch_begin(session.as_ref(), turn.as_ref(), &call_id, &changes).await;
                let result = fs::write(&path, &content).await.map_err(io_error).map(|_| {
                    FunctionToolOutput::from_text(format!("wrote {}", path.display()), Some(true))
                });
                if result.is_ok() {
                    record_observed_ranges(&tracker, &path, &content, whole_file_range(&content))
                        .await;
                }
                emit_patch_end(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    changes,
                    &result,
                    "wrote",
                )
                .await;
                result
            }
            "delete" => {
                let args: DeleteArgs = parse_arguments(&arguments)?;
                let path = resolve_path(turn.cwd.as_path(), &args.path);
                ensure_write_allowed(turn.as_ref(), &path).await?;
                let metadata = fs::metadata(&path).await.map_err(io_error)?;
                let actual_type = if metadata.is_dir() {
                    DeleteExpectedType::Dir
                } else {
                    DeleteExpectedType::File
                };
                if args
                    .expected_type
                    .is_some_and(|expected_type| expected_type != actual_type)
                {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "refusing to delete `{}` because expected_type does not match",
                        path.display()
                    )));
                }
                if args.dry_run {
                    return Ok(FunctionToolOutput::from_text(
                        format!("would delete {}", path.display()),
                        Some(true),
                    ));
                }
                let content = if metadata.is_dir() {
                    String::new()
                } else {
                    fs::read_to_string(&path).await.unwrap_or_default()
                };
                let changes = single_delete_change(&path, content);
                emit_patch_begin(session.as_ref(), turn.as_ref(), &call_id, &changes).await;
                let result = async {
                    if metadata.is_dir() {
                        if !args.recursive {
                            return Err(FunctionCallError::RespondToModel(
                                "refusing to delete directory without recursive=true".to_string(),
                            ));
                        }
                        fs::remove_dir_all(&path).await.map_err(io_error)?;
                    } else {
                        fs::remove_file(&path).await.map_err(io_error)?;
                    }
                    Ok(FunctionToolOutput::from_text(
                        format!("deleted {}", path.display()),
                        Some(true),
                    ))
                }
                .await;
                emit_patch_end(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    changes,
                    &result,
                    "deleted",
                )
                .await;
                result
            }
            other => Err(FunctionCallError::RespondToModel(format!(
                "unsupported file function {other}"
            ))),
        }
    }
}

async fn emit_tool_begin(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    command: Vec<String>,
    parsed_cmd: Vec<ParsedCommand>,
) -> Instant {
    session
        .send_event(
            turn,
            EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                call_id: call_id.to_string(),
                process_id: None,
                turn_id: turn.sub_id.clone(),
                command,
                cwd: turn.cwd.clone(),
                parsed_cmd,
                source: ExecCommandSource::Agent,
                run_mode: None,
                interaction_input: None,
            }),
        )
        .await;
    Instant::now()
}

async fn emit_tool_end(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    command: Vec<String>,
    parsed_cmd: Vec<ParsedCommand>,
    started: Instant,
    result: &Result<FunctionToolOutput, FunctionCallError>,
) {
    let (status, exit_code, output) = match result {
        Ok(_) => (ExecCommandStatus::Completed, 0, String::new()),
        Err(err) => (ExecCommandStatus::Failed, 1, err.to_string()),
    };
    session
        .send_event(
            turn,
            EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: call_id.to_string(),
                process_id: None,
                turn_id: turn.sub_id.clone(),
                command,
                cwd: turn.cwd.clone(),
                parsed_cmd,
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: if exit_code == 0 {
                    output.clone()
                } else {
                    String::new()
                },
                stderr: if exit_code == 0 {
                    String::new()
                } else {
                    output.clone()
                },
                aggregated_output: output.clone(),
                exit_code,
                duration: started.elapsed(),
                formatted_output: output,
                status,
            }),
        )
        .await;
}

async fn emit_patch_begin(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    changes: &HashMap<PathBuf, FileChange>,
) {
    session
        .send_event(
            turn,
            EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: call_id.to_string(),
                turn_id: turn.sub_id.clone(),
                auto_approved: true,
                changes: changes.clone(),
            }),
        )
        .await;
}

async fn emit_patch_end(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    changes: HashMap<PathBuf, FileChange>,
    result: &Result<FunctionToolOutput, FunctionCallError>,
    stdout: &str,
) {
    let (success, stderr) = match result {
        Ok(_) => (true, String::new()),
        Err(err) => (false, err.to_string()),
    };
    session
        .send_event(
            turn,
            EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: call_id.to_string(),
                turn_id: turn.sub_id.clone(),
                stdout: if success {
                    stdout.to_string()
                } else {
                    String::new()
                },
                stderr,
                success,
                changes,
                status: if success {
                    PatchApplyStatus::Completed
                } else {
                    PatchApplyStatus::Failed
                },
            }),
        )
        .await;
}

fn single_update_change(path: &Path, old: &str, new: &str) -> HashMap<PathBuf, FileChange> {
    HashMap::from([(
        path.to_path_buf(),
        FileChange::Update {
            unified_diff: unified_update_diff(path, old, new),
            move_path: None,
        },
    )])
}

fn unified_update_diff(path: &Path, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .header(&path.display().to_string(), &path.display().to_string())
        .to_string()
}

fn single_write_change(
    path: &Path,
    previous: Option<&str>,
    content: &str,
) -> HashMap<PathBuf, FileChange> {
    match previous {
        Some(previous) => single_update_change(path, previous, content),
        None => HashMap::from([(
            path.to_path_buf(),
            FileChange::Add {
                content: content.to_string(),
            },
        )]),
    }
}

fn single_delete_change(path: &Path, content: String) -> HashMap<PathBuf, FileChange> {
    HashMap::from([(path.to_path_buf(), FileChange::Delete { content })])
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn roots_or_cwd(cwd: &Path, roots: Vec<String>) -> Vec<PathBuf> {
    if roots.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        roots
            .into_iter()
            .map(|root| resolve_path(cwd, &root))
            .collect()
    }
}

fn ensure_read_allowed(
    turn: &crate::session::turn_context::TurnContext,
    path: &Path,
) -> Result<(), FunctionCallError> {
    let policy = turn.file_system_sandbox_policy();
    let matcher = ReadDenyMatcher::new(&policy, turn.cwd.as_path());
    if matcher
        .as_ref()
        .is_some_and(|matcher| matcher.is_read_denied(path))
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "access denied: reading `{}` is blocked by filesystem deny_read policy",
            path.display()
        )));
    }
    Ok(())
}

async fn ensure_write_allowed(
    turn: &crate::session::turn_context::TurnContext,
    path: &Path,
) -> Result<(), FunctionCallError> {
    let policy = turn.file_system_sandbox_policy();
    if !policy.can_write_path_with_cwd(path, turn.cwd.as_path()) {
        return Err(FunctionCallError::RespondToModel(format!(
            "access denied: writing `{}` is blocked by filesystem policy",
            path.display()
        )));
    }
    if let Some(resolved_path) = resolve_existing_write_target(path).await?
        && !policy.can_write_path_with_cwd(&resolved_path, turn.cwd.as_path())
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "access denied: writing `{}` resolves to `{}` which is blocked by filesystem policy",
            path.display(),
            resolved_path.display()
        )));
    }
    Ok(())
}

async fn resolve_existing_write_target(path: &Path) -> Result<Option<PathBuf>, FunctionCallError> {
    if fs::symlink_metadata(path).await.is_ok() {
        return fs::canonicalize(path).await.map(Some).map_err(io_error);
    }

    let mut missing_suffix = PathBuf::new();
    let mut ancestor = path;
    while let Some(parent) = ancestor.parent() {
        let Some(name) = ancestor.file_name() else {
            break;
        };
        missing_suffix = PathBuf::from(name).join(missing_suffix);
        if fs::symlink_metadata(parent).await.is_ok() {
            let resolved_parent = fs::canonicalize(parent).await.map_err(io_error)?;
            return Ok(Some(resolved_parent.join(missing_suffix)));
        }
        ancestor = parent;
    }

    Ok(None)
}

fn read_lines<'a>(
    text: &'a str,
    args: &ReadFileArgs,
) -> Result<Vec<(usize, &'a str)>, FunctionCallError> {
    if args.start_line.is_some() && args.offset != 0 {
        return Err(FunctionCallError::RespondToModel(
            "use either start_line/end_line or offset/limit, not both".to_string(),
        ));
    }
    if args.end_line.is_some() && args.start_line.is_none() {
        return Err(FunctionCallError::RespondToModel(
            "end_line requires start_line".to_string(),
        ));
    }

    let start_line = args.start_line.unwrap_or(args.offset + 1);
    if start_line == 0 {
        return Err(FunctionCallError::RespondToModel(
            "start_line must be one-based".to_string(),
        ));
    }
    let limit = match args.end_line {
        Some(end_line) if end_line < start_line => {
            return Err(FunctionCallError::RespondToModel(format!(
                "end_line {end_line} must be greater than or equal to start_line {start_line}"
            )));
        }
        Some(end_line) => end_line - start_line + 1,
        None => args.limit,
    };

    Ok(text
        .lines()
        .enumerate()
        .skip(start_line - 1)
        .take(limit)
        .map(|(index, line)| (index + 1, line))
        .collect())
}

fn format_read_lines(lines: &[(usize, &str)], include_line_numbers: bool) -> String {
    if include_line_numbers {
        lines
            .iter()
            .map(|(line_number, line)| format!("{line_number}:{line}"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        lines
            .iter()
            .map(|(_, line)| (*line).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn numbered_line_ranges(lines: &[(usize, &str)]) -> Vec<(usize, usize)> {
    lines
        .iter()
        .map(|(line_number, _)| (*line_number, *line_number))
        .collect()
}

async fn record_observed_ranges(
    tracker: &SharedTurnDiffTracker,
    path: &Path,
    text: &str,
    ranges: Vec<(usize, usize)>,
) {
    tracker
        .lock()
        .await
        .record_file_observation(path, content_hash(text), ranges);
}

async fn record_file_line_observations(
    tracker: &SharedTurnDiffTracker,
    observations: Vec<FileLineObservation>,
) {
    let mut tracker = tracker.lock().await;
    for observation in observations {
        let ranges = observation
            .lines
            .into_iter()
            .map(|line| (line, line))
            .collect::<Vec<_>>();
        tracker.record_file_observation(&observation.path, observation.content_hash, ranges);
    }
}

async fn record_edit_observation(
    tracker: &SharedTurnDiffTracker,
    path: &Path,
    previous_hash: &str,
    edited: &str,
    line_origins: &[Option<usize>],
    ranges: Vec<(usize, usize)>,
) {
    tracker.lock().await.record_file_edit_observation(
        path,
        previous_hash,
        content_hash(edited),
        line_origins,
        ranges,
    );
}

fn whole_file_range(text: &str) -> Vec<(usize, usize)> {
    let line_count = file_line_count(text);
    if line_count == 0 {
        Vec::new()
    } else {
        vec![(1, line_count)]
    }
}

async fn ensure_ranges_observed_for_edit(
    tracker: &SharedTurnDiffTracker,
    path: &Path,
    content_hash: &str,
    ranges: &[(usize, usize)],
) -> Result<(), FunctionCallError> {
    tracker
        .lock()
        .await
        .verify_file_observation(path, content_hash, ranges)
        .map_err(|err| observation_error(path, err))
}

fn observation_error(path: &Path, err: FileObservationError) -> FunctionCallError {
    let message = match err {
        FileObservationError::NotObserved => format!(
            "refusing to edit `{}` because the target lines have not been read or searched this turn",
            path.display()
        ),
        FileObservationError::Stale {
            observed_hash,
            current_hash,
        } => format!(
            "refusing to edit `{}` because it changed since it was read or searched: observed {observed_hash}, actual {current_hash}",
            path.display()
        ),
        FileObservationError::MissingLines { ranges } => format!(
            "refusing to edit `{}` because lines {} have not been read or searched this turn",
            path.display(),
            format_line_ranges(&ranges)
        ),
    };
    FunctionCallError::RespondToModel(message)
}

fn format_line_ranges(ranges: &[(usize, usize)]) -> String {
    ranges
        .iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn ensure_expected_hash(text: &str, expected_hash: &str) -> Result<(), FunctionCallError> {
    let actual_hash = content_hash(text);
    if expected_hash != actual_hash {
        return Err(FunctionCallError::RespondToModel(format!(
            "expected_hash mismatch: expected {expected_hash}, actual {actual_hash}"
        )));
    }
    Ok(())
}

fn content_hash(text: &str) -> String {
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(text.as_bytes());
    format!("sha1:{:x}", hasher.finalize())
}

async fn collect_path_matches(
    turn: &crate::session::turn_context::TurnContext,
    root: &Path,
    query: &str,
    filters: PathFilters<'_>,
    limit: usize,
    matches: &mut Vec<SearchMatch>,
) -> Result<(), FunctionCallError> {
    let mut stack = vec![root.to_path_buf()];
    let query = query.to_lowercase();
    while let Some(path) = stack.pop() {
        ensure_read_allowed(turn, &path)?;
        let metadata = match fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            if is_default_excluded_dir(&path) {
                continue;
            }
            let mut entries = fs::read_dir(&path).await.map_err(io_error)?;
            while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
                stack.push(entry.path());
            }
            continue;
        }
        let display = path.display().to_string();
        let relative = path.strip_prefix(root).ok().map(path_to_forward_slashes);
        if path_allowed(&display, relative.as_deref(), filters)
            && let Some(score) = fuzzy_score(&display, &query)
        {
            matches.push(SearchMatch {
                path: display,
                relative_path: relative,
                score,
            });
            if matches.len() >= limit {
                break;
            }
        }
    }
    Ok(())
}

fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(i32::MAX);
    }

    let haystack = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    let mut first = None;
    let mut last = 0usize;
    let mut haystack_chars = haystack.chars().enumerate();
    for needle_char in needle.chars() {
        let (index, _) = haystack_chars.find(|(_, ch)| *ch == needle_char)?;
        first.get_or_insert(index);
        last = index;
    }
    let first = first.unwrap_or(0);
    Some((last.saturating_sub(first) as i32) - (needle.chars().count() as i32))
}

async fn collect_grep_text_matches(
    turn: &crate::session::turn_context::TurnContext,
    root: &Path,
    options: &GrepCollectOptions<'_>,
    matches: &mut Vec<String>,
    observations: &mut Vec<FileLineObservation>,
) -> Result<(), FunctionCallError> {
    let mut files = Vec::new();
    collect_files(turn, root, options.filters, usize::MAX, &mut files).await?;
    for file in files {
        let Ok(text) = fs::read_to_string(&file).await else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        let mut observed_lines = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if !options.regex.is_match(line) {
                continue;
            }
            let start = index.saturating_sub(options.context);
            let end = (index + options.context + 1).min(lines.len());
            for (line_index, line) in lines[start..end].iter().enumerate() {
                let line_number = start + line_index + 1;
                matches.push(format!("{}:{}:{}", file.display(), line_number, line));
                observed_lines.push(line_number);
            }
            if matches.len() >= options.limit {
                push_file_line_observation(observations, file, &text, observed_lines);
                return Ok(());
            }
        }
        push_file_line_observation(observations, file, &text, observed_lines);
    }
    Ok(())
}

async fn collect_grep_structured_matches(
    turn: &crate::session::turn_context::TurnContext,
    root: &Path,
    options: &GrepCollectOptions<'_>,
    matches: &mut Vec<GrepMatch>,
    observations: &mut Vec<FileLineObservation>,
) -> Result<(), FunctionCallError> {
    let mut files = Vec::new();
    collect_files(turn, root, options.filters, usize::MAX, &mut files).await?;
    for file in files {
        let Ok(text) = fs::read_to_string(&file).await else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        let mut observed_lines = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if !options.regex.is_match(line) {
                continue;
            }
            let before_start = index.saturating_sub(options.context);
            let context_before = lines[before_start..index]
                .iter()
                .enumerate()
                .map(|(context_index, line)| {
                    let line_number = before_start + context_index + 1;
                    observed_lines.push(line_number);
                    GrepContextLine {
                        line: line_number,
                        text: (*line).to_string(),
                    }
                })
                .collect();
            let after_end = (index + options.context + 1).min(lines.len());
            let context_after = lines[index + 1..after_end]
                .iter()
                .enumerate()
                .map(|(context_index, line)| {
                    let line_number = index + context_index + 2;
                    observed_lines.push(line_number);
                    GrepContextLine {
                        line: line_number,
                        text: (*line).to_string(),
                    }
                })
                .collect();
            observed_lines.push(index + 1);
            matches.push(GrepMatch {
                path: file.display().to_string(),
                line: index + 1,
                text: (*line).to_string(),
                context_before,
                context_after,
            });
            if matches.len() >= options.limit {
                push_file_line_observation(observations, file, &text, observed_lines);
                return Ok(());
            }
        }
        push_file_line_observation(observations, file, &text, observed_lines);
    }
    Ok(())
}

fn push_file_line_observation(
    observations: &mut Vec<FileLineObservation>,
    path: PathBuf,
    text: &str,
    lines: Vec<usize>,
) {
    if lines.is_empty() {
        return;
    }
    observations.push(FileLineObservation {
        path,
        content_hash: content_hash(text),
        lines,
    });
}

async fn collect_glob_matches(
    turn: &crate::session::turn_context::TurnContext,
    root: &Path,
    pattern: &str,
    entry_type: GlobEntryType,
    limit: usize,
    matches: &mut Vec<GlobMatch>,
) -> Result<(), FunctionCallError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        ensure_read_allowed(turn, &path)?;
        let metadata = match fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() && !is_default_excluded_dir(&path) {
            let mut entries = fs::read_dir(&path).await.map_err(io_error)?;
            while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
                stack.push(entry.path());
            }
        }

        let actual_entry_type = if metadata.is_dir() {
            GlobEntryType::Dir
        } else {
            GlobEntryType::File
        };
        if entry_type != GlobEntryType::Any && entry_type != actual_entry_type {
            continue;
        }

        let display = path.display().to_string();
        let relative = path.strip_prefix(root).ok().map(path_to_forward_slashes);
        if glob_match(pattern, &display)
            || relative
                .as_deref()
                .is_some_and(|relative| glob_match(pattern, relative))
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| glob_match(pattern, name))
        {
            matches.push(GlobMatch {
                path: display,
                relative_path: relative,
                entry_type: match actual_entry_type {
                    GlobEntryType::File => "file",
                    GlobEntryType::Dir => "dir",
                    GlobEntryType::Any => "unknown",
                },
            });
            if matches.len() >= limit {
                break;
            }
        }
    }
    Ok(())
}

async fn collect_files(
    turn: &crate::session::turn_context::TurnContext,
    root: &Path,
    filters: PathFilters<'_>,
    cap: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), FunctionCallError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        ensure_read_allowed(turn, &path)?;
        let metadata = match fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            if is_default_excluded_dir(&path) {
                continue;
            }
            let mut entries = fs::read_dir(&path).await.map_err(io_error)?;
            while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
                stack.push(entry.path());
            }
        } else {
            let display = path.display().to_string();
            let relative = path.strip_prefix(root).ok().map(path_to_forward_slashes);
            if path_allowed(&display, relative.as_deref(), filters) {
                files.push(path);
                if files.len() >= cap {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn path_allowed(path: &str, relative_path: Option<&str>, filters: PathFilters<'_>) -> bool {
    (filters.include.is_empty() || filters.include.iter().any(|needle| path.contains(needle)))
        && !filters.exclude.iter().any(|needle| path.contains(needle))
        && (filters.include_globs.is_empty()
            || filters
                .include_globs
                .iter()
                .any(|pattern| path_matches_glob(pattern, path, relative_path)))
        && !filters
            .exclude_globs
            .iter()
            .any(|pattern| path_matches_glob(pattern, path, relative_path))
}

fn path_matches_glob(pattern: &str, path: &str, relative_path: Option<&str>) -> bool {
    glob_match(pattern, path)
        || relative_path.is_some_and(|relative_path| glob_match(pattern, relative_path))
        || Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| glob_match(pattern, name))
}

fn glob_filter_warning(include: &[String], exclude: &[String]) -> Option<String> {
    let glob_like = include
        .iter()
        .chain(exclude)
        .find(|value| looks_like_glob(value))?;
    Some(format!(
        "warning: include/exclude use substring matching, not glob matching; use include_globs/exclude_globs for patterns like `{glob_like}`"
    ))
}

fn looks_like_glob(value: &str) -> bool {
    value.contains('*') || value.contains('?') || value.contains('[')
}

fn text_output_with_warnings(warning: Option<String>, output: String) -> String {
    match (warning, output.is_empty()) {
        (Some(warning), true) => warning,
        (Some(warning), false) => format!("{warning}\n{output}"),
        (None, _) => output,
    }
}

fn is_default_excluded_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| DEFAULT_EXCLUDED_DIRS.contains(&name))
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    regex.push_str("(?:.*/)?");
                } else {
                    regex.push_str(".*");
                }
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                regex.push('\\');
                regex.push(ch);
            }
            ch => regex.push(ch),
        }
    }
    regex.push('$');
    regex_lite::Regex::new(&regex).is_ok_and(|regex| regex.is_match(text))
}

fn edit_request(args: EditArgs) -> Result<EditRequest, FunctionCallError> {
    let operations = if !args.ops.is_empty() {
        if args.old_text.is_some() || args.new_text.is_some() || args.occurrence.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "use either ops or legacy old_text/new_text fields, not both".to_string(),
            ));
        }
        args.ops
    } else {
        let old_text = args.old_text.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "old_text is required when ops is omitted".to_string(),
            )
        })?;
        let new_text = args.new_text.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "new_text is required when ops is omitted".to_string(),
            )
        })?;
        vec![EditOperation::Replace {
            old_text,
            new_text,
            occurrence: args.occurrence,
        }]
    };

    Ok(EditRequest {
        apply_mode: args.apply_mode,
        operations,
    })
}

fn sequential_edit_warning(
    operations: &[EditOperation],
    apply_mode: EditApplyMode,
) -> Option<String> {
    if apply_mode != EditApplyMode::Sequential {
        return None;
    }

    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (index, operation) in operations.iter().enumerate() {
        let old_text = match operation {
            EditOperation::Replace { old_text, .. }
            | EditOperation::DeleteText { old_text, .. }
            | EditOperation::ReplaceOccurrences { old_text, .. } => old_text.as_str(),
            EditOperation::InsertBefore { .. }
            | EditOperation::InsertAfter { .. }
            | EditOperation::ReplaceRange { .. }
            | EditOperation::DeleteRange { .. } => continue,
        };
        if seen.insert(old_text, index + 1).is_some() {
            return Some(format!(
                "warning: op {} occurrence resolved against modified buffer in sequential mode. Use apply_mode=\"snapshot\" for simultaneous replacement.",
                index + 1
            ));
        }
    }
    None
}

struct EditObservationPlan {
    required_ranges: Vec<(usize, usize)>,
    final_line_origins: Vec<Option<usize>>,
}

fn edit_observation_plan(
    text: &str,
    request: &EditRequest,
) -> Result<EditObservationPlan, FunctionCallError> {
    match request.apply_mode {
        EditApplyMode::Sequential => edit_sequential_observation_plan(text, &request.operations),
        EditApplyMode::Snapshot => edit_snapshot_observation_plan(text, &request.operations),
    }
}

fn edit_sequential_observation_plan(
    text: &str,
    operations: &[EditOperation],
) -> Result<EditObservationPlan, FunctionCallError> {
    let mut required_ranges = Vec::new();
    let mut current_text = text.to_string();
    let mut line_origins = original_line_origins(text);
    for (index, operation) in operations.iter().enumerate() {
        let observed_ranges = edit_operation_observed_ranges(&current_text, operation)
            .map_err(|err| prefix_edit_operation_error(err, index, operations.len()))?;
        for (start_line, end_line) in &observed_ranges {
            required_ranges.extend(original_ranges_for_current_lines(
                &line_origins,
                *start_line,
                *end_line,
            ));
        }
        update_line_origins(&mut line_origins, operation, &observed_ranges);
        current_text = apply_edit_operation(&current_text, operation, index)
            .map_err(|err| prefix_edit_operation_error(err, index, operations.len()))?
            .text;
    }
    merge_line_ranges(&mut required_ranges);
    Ok(EditObservationPlan {
        required_ranges,
        final_line_origins: line_origins,
    })
}

fn edit_snapshot_observation_plan(
    text: &str,
    operations: &[EditOperation],
) -> Result<EditObservationPlan, FunctionCallError> {
    let resolved = resolve_snapshot_edits(text, operations)?;
    let mut required_ranges = resolved
        .iter()
        .map(|edit| byte_range_to_line_range(text, edit.range.start, edit.range.end))
        .collect::<Vec<_>>();
    merge_line_ranges(&mut required_ranges);
    let edited = apply_resolved_edits(text, &resolved);
    Ok(EditObservationPlan {
        required_ranges,
        final_line_origins: vec![None; file_line_count(&edited)],
    })
}

fn prefix_edit_operation_error(
    err: FunctionCallError,
    index: usize,
    operation_count: usize,
) -> FunctionCallError {
    match err {
        FunctionCallError::RespondToModel(message) if operation_count > 1 => {
            FunctionCallError::RespondToModel(format!("op {}: {message}", index + 1))
        }
        err => err,
    }
}

fn original_line_origins(text: &str) -> Vec<Option<usize>> {
    (1..=file_line_count(text)).map(Some).collect()
}

fn original_ranges_for_current_lines(
    line_origins: &[Option<usize>],
    start_line: usize,
    end_line: usize,
) -> Vec<(usize, usize)> {
    let mut ranges = line_origins[start_line - 1..end_line]
        .iter()
        .filter_map(|line| *line)
        .map(|line| (line, line))
        .collect::<Vec<_>>();
    merge_line_ranges(&mut ranges);
    ranges
}

fn merge_line_ranges(ranges: &mut Vec<(usize, usize)>) {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges.drain(..) {
        let Some((_, last_end)) = merged.last_mut() else {
            merged.push((start, end));
            continue;
        };
        if start <= last_end.saturating_add(1) {
            *last_end = (*last_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    *ranges = merged;
}

fn update_line_origins(
    line_origins: &mut Vec<Option<usize>>,
    operation: &EditOperation,
    observed_ranges: &[(usize, usize)],
) {
    let Some(&(start_line, end_line)) = observed_ranges.first() else {
        return;
    };

    match operation {
        EditOperation::Replace {
            old_text, new_text, ..
        } => {
            update_text_replacement_origins(line_origins, start_line, end_line, old_text, new_text)
        }
        EditOperation::DeleteText { old_text, .. } => {
            update_text_replacement_origins(line_origins, start_line, end_line, old_text, "")
        }
        EditOperation::ReplaceOccurrences {
            old_text, new_text, ..
        } => {
            for (start_line, end_line) in observed_ranges.iter().rev() {
                update_text_replacement_origins(
                    line_origins,
                    *start_line,
                    *end_line,
                    old_text,
                    new_text,
                );
            }
        }
        EditOperation::DeleteRange { .. } => {
            line_origins.splice(start_line - 1..end_line, Vec::new());
        }
        EditOperation::ReplaceRange { new_text, .. } => {
            replace_line_origins(line_origins, start_line, end_line, new_text)
        }
        EditOperation::InsertBefore { text, .. } => {
            let insert_count = inserted_line_count(text);
            line_origins.splice(start_line - 1..start_line - 1, vec![None; insert_count]);
        }
        EditOperation::InsertAfter { text, .. } => {
            let insert_count = inserted_line_count(text);
            line_origins.splice(end_line..end_line, vec![None; insert_count]);
        }
    }
}

fn update_text_replacement_origins(
    line_origins: &mut Vec<Option<usize>>,
    start_line: usize,
    end_line: usize,
    old_text: &str,
    new_text: &str,
) {
    if start_line == end_line && !old_text.contains('\n') && !new_text.contains('\n') {
        return;
    }
    replace_line_origins(line_origins, start_line, end_line, new_text);
}

fn replace_line_origins(
    line_origins: &mut Vec<Option<usize>>,
    start_line: usize,
    end_line: usize,
    new_text: &str,
) {
    let origin = line_origins[start_line - 1];
    let replacement = vec![origin; line_count_for_fragment(new_text)];
    line_origins.splice(start_line - 1..end_line, replacement);
}

fn inserted_line_count(text: &str) -> usize {
    text.as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
}

fn line_count_for_fragment(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.split_inclusive('\n').count()
    }
}

fn file_line_count(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

fn changed_line_ranges(old: &str, new: &str) -> Vec<(usize, usize)> {
    let new_line_count = file_line_count(new);
    let mut new_line = 1usize;
    let mut ranges = Vec::new();
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                new_line += 1;
            }
            ChangeTag::Insert => {
                ranges.push((new_line, new_line));
                new_line += 1;
            }
            ChangeTag::Delete => {
                if new_line_count != 0 {
                    let line = new_line.min(new_line_count);
                    ranges.push((line, line));
                }
            }
        }
    }
    merge_line_ranges(&mut ranges);
    ranges
}

fn edit_operation_observed_ranges(
    text: &str,
    operation: &EditOperation,
) -> Result<Vec<(usize, usize)>, FunctionCallError> {
    match operation {
        EditOperation::Replace {
            old_text,
            occurrence,
            ..
        }
        | EditOperation::DeleteText {
            old_text,
            occurrence,
        } => Ok(vec![text_occurrence_line_range(
            text,
            old_text,
            *occurrence,
            "old_text",
        )?]),
        EditOperation::ReplaceOccurrences {
            old_text,
            occurrences,
            ..
        } => occurrences
            .iter()
            .map(|occurrence| {
                text_occurrence_line_range(text, old_text, Some(*occurrence), "old_text")
            })
            .collect(),
        EditOperation::InsertBefore {
            anchor, occurrence, ..
        }
        | EditOperation::InsertAfter {
            anchor, occurrence, ..
        } => Ok(vec![text_occurrence_line_range(
            text,
            anchor,
            *occurrence,
            "anchor",
        )?]),
        EditOperation::ReplaceRange {
            start_line,
            end_line,
            ..
        }
        | EditOperation::DeleteRange {
            start_line,
            end_line,
        } => Ok(vec![validate_line_range(text, *start_line, *end_line)?]),
    }
}

fn text_occurrence_line_range(
    text: &str,
    needle: &str,
    occurrence: Option<usize>,
    field_name: &str,
) -> Result<(usize, usize), FunctionCallError> {
    let range = text_occurrence_byte_range(text, needle, occurrence, field_name)?;
    Ok(byte_range_to_line_range(text, range.start, range.end))
}

fn text_occurrence_byte_range(
    text: &str,
    needle: &str,
    occurrence: Option<usize>,
    field_name: &str,
) -> Result<std::ops::Range<usize>, FunctionCallError> {
    let occurrence = resolve_occurrence(text, needle, occurrence, field_name)?;
    let start = occurrence_start(text, needle, occurrence)
        .ok_or_else(|| FunctionCallError::RespondToModel(format!("{field_name} was not found")))?;
    Ok(start..start + needle.len())
}

fn occurrence_start(text: &str, needle: &str, occurrence: usize) -> Option<usize> {
    let mut start = 0;
    for _ in 1..occurrence {
        let index = text[start..].find(needle)?;
        start += index + needle.len();
    }
    text[start..].find(needle).map(|index| start + index)
}

fn byte_range_to_line_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let start_line = line_number_at_byte(text, start);
    let end_line = line_number_at_byte(text, end.saturating_sub(1).max(start));
    (start_line, end_line)
}

fn line_number_at_byte(text: &str, byte_index: usize) -> usize {
    text.as_bytes()
        .iter()
        .take(byte_index.min(text.len()))
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn validate_line_range(
    text: &str,
    start_line: usize,
    end_line: usize,
) -> Result<(usize, usize), FunctionCallError> {
    if start_line == 0 || end_line == 0 {
        return Err(FunctionCallError::RespondToModel(
            "line ranges must be one-based".to_string(),
        ));
    }
    if start_line > end_line {
        return Err(FunctionCallError::RespondToModel(format!(
            "start_line {start_line} must be less than or equal to end_line {end_line}"
        )));
    }
    let line_count = file_line_count(text);
    if end_line > line_count {
        return Err(FunctionCallError::RespondToModel(format!(
            "line range {start_line}-{end_line} is out of range; file has {line_count} lines"
        )));
    }
    Ok((start_line, end_line))
}

struct AppliedEdit {
    text: String,
    hits: Vec<EditHit>,
}

#[derive(Clone)]
struct ResolvedEdit {
    op_index: usize,
    range: std::ops::Range<usize>,
    replacement: String,
}

struct EditHit {
    op_index: usize,
    range: std::ops::Range<usize>,
}

fn apply_edit_operations(
    text: &str,
    request: &EditRequest,
) -> Result<AppliedEdit, FunctionCallError> {
    match request.apply_mode {
        EditApplyMode::Sequential => apply_sequential_edit_operations(text, &request.operations),
        EditApplyMode::Snapshot => {
            let resolved = resolve_snapshot_edits(text, &request.operations)?;
            Ok(AppliedEdit {
                text: apply_resolved_edits(text, &resolved),
                hits: edit_hits(&resolved),
            })
        }
    }
}

fn apply_sequential_edit_operations(
    text: &str,
    operations: &[EditOperation],
) -> Result<AppliedEdit, FunctionCallError> {
    let mut edited = text.to_string();
    let mut hits = Vec::new();
    for (index, operation) in operations.iter().enumerate() {
        let applied = apply_edit_operation(&edited, operation, index).map_err(|err| match err {
            FunctionCallError::RespondToModel(message) if operations.len() > 1 => {
                FunctionCallError::RespondToModel(format!("op {}: {message}", index + 1))
            }
            err => err,
        })?;
        edited = applied.text;
        hits.extend(applied.hits);
    }
    Ok(AppliedEdit { text: edited, hits })
}

fn apply_edit_operation(
    text: &str,
    operation: &EditOperation,
    op_index: usize,
) -> Result<AppliedEdit, FunctionCallError> {
    let resolved = resolve_operation_edits(text, operation, op_index)?;
    ensure_non_overlapping(&resolved)?;
    Ok(AppliedEdit {
        text: apply_resolved_edits(text, &resolved),
        hits: edit_hits(&resolved),
    })
}

fn resolve_snapshot_edits(
    text: &str,
    operations: &[EditOperation],
) -> Result<Vec<ResolvedEdit>, FunctionCallError> {
    let mut edits = Vec::new();
    for (index, operation) in operations.iter().enumerate() {
        edits.extend(
            resolve_operation_edits(text, operation, index)
                .map_err(|err| prefix_edit_operation_error(err, index, operations.len()))?,
        );
    }
    ensure_non_overlapping(&edits)?;
    Ok(edits)
}

fn resolve_operation_edits(
    text: &str,
    operation: &EditOperation,
    op_index: usize,
) -> Result<Vec<ResolvedEdit>, FunctionCallError> {
    match operation {
        EditOperation::Replace {
            old_text,
            new_text,
            occurrence,
        } => Ok(vec![ResolvedEdit {
            op_index,
            range: text_occurrence_byte_range(text, old_text, *occurrence, "old_text")?,
            replacement: new_text.clone(),
        }]),
        EditOperation::InsertBefore {
            anchor,
            text: insert_text,
            occurrence,
        } => Ok(vec![ResolvedEdit {
            op_index,
            range: text_occurrence_byte_range(text, anchor, *occurrence, "anchor")?,
            replacement: format!("{insert_text}{anchor}"),
        }]),
        EditOperation::InsertAfter {
            anchor,
            text: insert_text,
            occurrence,
        } => Ok(vec![ResolvedEdit {
            op_index,
            range: text_occurrence_byte_range(text, anchor, *occurrence, "anchor")?,
            replacement: format!("{anchor}{insert_text}"),
        }]),
        EditOperation::ReplaceRange {
            start_line,
            end_line,
            new_text,
        } => Ok(vec![ResolvedEdit {
            op_index,
            range: line_byte_range(text, *start_line, *end_line)?,
            replacement: new_text.clone(),
        }]),
        EditOperation::DeleteRange {
            start_line,
            end_line,
        } => Ok(vec![ResolvedEdit {
            op_index,
            range: line_byte_range(text, *start_line, *end_line)?,
            replacement: String::new(),
        }]),
        EditOperation::DeleteText {
            old_text,
            occurrence,
        } => Ok(vec![ResolvedEdit {
            op_index,
            range: text_occurrence_byte_range(text, old_text, *occurrence, "old_text")?,
            replacement: String::new(),
        }]),
        EditOperation::ReplaceOccurrences {
            old_text,
            new_text,
            occurrences,
        } => {
            if occurrences.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "occurrences must not be empty".to_string(),
                ));
            }
            occurrences
                .iter()
                .map(|occurrence| {
                    Ok(ResolvedEdit {
                        op_index,
                        range: text_occurrence_byte_range(
                            text,
                            old_text,
                            Some(*occurrence),
                            "old_text",
                        )?,
                        replacement: new_text.clone(),
                    })
                })
                .collect()
        }
    }
}

fn line_byte_range(
    text: &str,
    start_line: usize,
    end_line: usize,
) -> Result<std::ops::Range<usize>, FunctionCallError> {
    validate_line_range(text, start_line, end_line)?;
    let mut start = 0;
    for line in text.split_inclusive('\n').take(start_line - 1) {
        start += line.len();
    }
    let mut end = start;
    for line in text
        .split_inclusive('\n')
        .skip(start_line - 1)
        .take(end_line - start_line + 1)
    {
        end += line.len();
    }
    Ok(start..end)
}

fn ensure_non_overlapping(edits: &[ResolvedEdit]) -> Result<(), FunctionCallError> {
    let mut indexes = (0..edits.len()).collect::<Vec<_>>();
    indexes.sort_unstable_by_key(|index| {
        let range = &edits[*index].range;
        (range.start, range.end)
    });

    for pair in indexes.windows(2) {
        let previous = &edits[pair[0]];
        let current = &edits[pair[1]];
        if ranges_overlap(&previous.range, &current.range) {
            return Err(FunctionCallError::RespondToModel(format!(
                "op {} overlaps op {}; no changes written.\nop {}: bytes {}..{}\nop {}: bytes {}..{}",
                current.op_index + 1,
                previous.op_index + 1,
                previous.op_index + 1,
                previous.range.start,
                previous.range.end,
                current.op_index + 1,
                current.range.start,
                current.range.end
            )));
        }
    }

    Ok(())
}

fn ranges_overlap(a: &std::ops::Range<usize>, b: &std::ops::Range<usize>) -> bool {
    (a.start < b.end && b.start < a.end)
        || (a.start == a.end && b.start == b.end && a.start == b.start)
}

fn apply_resolved_edits(text: &str, edits: &[ResolvedEdit]) -> String {
    let mut edited = text.to_string();
    let mut indexes = (0..edits.len()).collect::<Vec<_>>();
    indexes.sort_unstable_by_key(|index| edits[*index].range.start);
    for index in indexes.into_iter().rev() {
        let edit = &edits[index];
        edited.replace_range(edit.range.clone(), &edit.replacement);
    }
    edited
}

fn edit_hits(edits: &[ResolvedEdit]) -> Vec<EditHit> {
    edits
        .iter()
        .map(|edit| EditHit {
            op_index: edit.op_index,
            range: edit.range.clone(),
        })
        .collect()
}

fn resolve_occurrence(
    text: &str,
    needle: &str,
    occurrence: Option<usize>,
    field_name: &str,
) -> Result<usize, FunctionCallError> {
    let count = text.matches(needle).count();
    match occurrence {
        Some(0) => Err(FunctionCallError::RespondToModel(
            "occurrence must be one-based".to_string(),
        )),
        Some(occurrence) if occurrence > count => Err(FunctionCallError::RespondToModel(format!(
            "{field_name} occurs {count} times; occurrence {occurrence} is out of range"
        ))),
        Some(occurrence) => Ok(occurrence),
        None if count == 1 => Ok(1),
        None if count == 0 => Err(FunctionCallError::RespondToModel(format!(
            "{field_name} was not found"
        ))),
        None => Err(FunctionCallError::RespondToModel(format!(
            "{field_name} occurs {count} times; specify occurrence"
        ))),
    }
}

fn edit_output(
    path: &Path,
    old: &str,
    new: &str,
    warning: Option<String>,
    hits: &[EditHit],
) -> String {
    let mut output = unified_update_diff(path, old, new);
    if !hits.is_empty() {
        output.push_str("\nMatched ranges:");
        for hit in hits {
            output.push_str(&format!(
                "\nop {}: bytes {}..{}",
                hit.op_index + 1,
                hit.range.start,
                hit.range.end
            ));
        }
    }
    text_output_with_warnings(warning, output)
}

fn truncate(text: String, max_output_tokens: Option<usize>) -> String {
    let max_tokens = resolve_max_tokens(max_output_tokens);
    formatted_truncate_text(&text, TruncationPolicy::Tokens(max_tokens))
}

fn io_error(err: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context;
    use crate::tools::context::ToolCallSource;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::models::SandboxEnforcement;
    use codex_protocol::protocol::FileSystemSandboxPolicy;
    use codex_protocol::protocol::NetworkSandboxPolicy;
    use codex_protocol::protocol::SandboxPolicy;
    use core_test_support::PathExt;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    async fn invocation(
        root: &TempDir,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> ToolInvocation {
        invocation_with_tracker(
            root,
            tool_name,
            arguments,
            Arc::new(Mutex::new(TurnDiffTracker::new())),
        )
        .await
    }

    async fn invocation_with_tracker(
        root: &TempDir,
        tool_name: &str,
        arguments: serde_json::Value,
        tracker: SharedTurnDiffTracker,
    ) -> ToolInvocation {
        let (session, mut turn) = make_session_and_context().await;
        turn.cwd = root.path().abs();
        let sandbox_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        };
        let file_system_sandbox_policy =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&sandbox_policy, &turn.cwd);
        let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
        turn.permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
            SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
            &file_system_sandbox_policy,
            network_sandbox_policy,
        );
        ToolInvocation {
            session: session.into(),
            turn: turn.into(),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker,
            call_id: format!("call-{tool_name}"),
            tool_name: codex_tools::ToolName::plain(tool_name),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: arguments.to_string(),
            },
        }
    }

    fn output_text(output: FunctionToolOutput) -> String {
        let [FunctionCallOutputContentItem::InputText { text }] = output.body.as_slice() else {
            panic!("expected one text output item");
        };
        text.clone()
    }

    #[test]
    fn read_file_display_command_keeps_range_arguments() {
        let args = ReadFileArgs {
            path: "sample.txt".to_string(),
            offset: 0,
            limit: DEFAULT_LIMIT,
            start_line: Some(85),
            end_line: Some(97),
            include_line_numbers: false,
            max_output_tokens: None,
        };
        assert_eq!(
            read_file_display_command(&args),
            "read_file --start-line=85 --end-line=97"
        );

        let args = ReadFileArgs {
            path: "sample.txt".to_string(),
            offset: 0,
            limit: 80,
            start_line: Some(1),
            end_line: None,
            include_line_numbers: false,
            max_output_tokens: None,
        };
        assert_eq!(
            read_file_display_command(&args),
            "read_file --start-line=1 --limit=80"
        );
    }

    #[tokio::test]
    async fn read_file_returns_requested_page() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("sample.txt"), "alpha\nbeta\ngamma\n").await?;

        let output = FileHandler
            .handle(
                invocation(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "offset": 1, "limit": 1 }),
                )
                .await,
            )
            .await?;

        assert_eq!(output_text(output), "beta");
        Ok(())
    }

    #[tokio::test]
    async fn read_file_supports_line_range_and_line_numbers() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("sample.txt"), "alpha\nbeta\ngamma\n").await?;

        let output = FileHandler
            .handle(
                invocation(
                    &root,
                    "read_file",
                    json!({
                        "path": "sample.txt",
                        "start_line": 2,
                        "end_line": 3,
                        "include_line_numbers": true
                    }),
                )
                .await,
            )
            .await?;

        assert_eq!(output_text(output), "2:beta\n3:gamma");
        Ok(())
    }

    #[tokio::test]
    async fn search_grep_and_glob_return_structured_output() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("src")).await?;
        fs::write(root.path().join("src/main.rs"), "alpha\nbeta\ngamma\n").await?;

        let search_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "search_file",
                    json!({
                        "query": "mainrs",
                        "limit": 5,
                        "include_globs": ["*.rs"],
                        "output_format": "json"
                    }),
                )
                .await,
            )
            .await?;
        let search_json: serde_json::Value = serde_json::from_str(&output_text(search_output))?;
        assert_eq!(search_json[0]["relative_path"], json!("src/main.rs"));
        assert!(search_json[0]["score"].is_number());

        let grep_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "grep_file",
                    json!({
                        "pattern": "beta",
                        "context": 1,
                        "output_format": "json"
                    }),
                )
                .await,
            )
            .await?;
        let grep_json: serde_json::Value = serde_json::from_str(&output_text(grep_output))?;
        assert_eq!(grep_json[0]["line"], json!(2));
        assert_eq!(grep_json[0]["context_before"][0]["text"], json!("alpha"));
        assert_eq!(grep_json[0]["context_after"][0]["text"], json!("gamma"));

        let glob_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "glob_file",
                    json!({
                        "pattern": "src/*.rs",
                        "relative_only": true,
                        "output_format": "json"
                    }),
                )
                .await,
            )
            .await?;
        let glob_json: serde_json::Value = serde_json::from_str(&output_text(glob_output))?;
        assert_eq!(glob_json[0]["relative_path"], json!("src/main.rs"));
        assert_eq!(glob_json[0]["entry_type"], json!("file"));
        Ok(())
    }

    #[tokio::test]
    async fn search_grep_and_glob_return_matches() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("src")).await?;
        fs::write(
            root.path().join("src/main.rs"),
            "fn main() { println!(\"alpha\"); }\n",
        )
        .await?;
        fs::write(root.path().join("README.md"), "omega\n").await?;
        fs::create_dir(root.path().join("target")).await?;
        fs::write(root.path().join("target/main.rs"), "fn generated() {}\n").await?;

        let search_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "search_file",
                    json!({ "query": "mainrs", "limit": 5 }),
                )
                .await,
            )
            .await?;
        let search_text = output_text(search_output);
        assert!(search_text.contains("src/main.rs"));
        assert!(!search_text.contains("target/main.rs"));

        let grep_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "grep_file",
                    json!({ "pattern": "println", "include_globs": ["*.rs"], "limit": 5 }),
                )
                .await,
            )
            .await?;
        assert!(output_text(grep_output).contains("src/main.rs:1:fn main()"));

        let warning_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "grep_file",
                    json!({ "pattern": "println", "include": ["*.rs"], "limit": 5 }),
                )
                .await,
            )
            .await?;
        assert_eq!(
            output_text(warning_output),
            "warning: include/exclude use substring matching, not glob matching; use include_globs/exclude_globs for patterns like `*.rs`"
        );

        let glob_output = FileHandler
            .handle(invocation(&root, "glob_file", json!({ "pattern": "*.md" })).await)
            .await?;
        assert!(output_text(glob_output).contains("README.md"));

        let relative_glob_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "glob_file",
                    json!({ "pattern": "src/*.rs", "root": root.path() }),
                )
                .await,
            )
            .await?;
        assert!(output_text(relative_glob_output).contains("src/main.rs"));

        let globstar_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "glob_file",
                    json!({ "pattern": "src/**/*main*.rs", "root": root.path() }),
                )
                .await,
            )
            .await?;
        assert!(output_text(globstar_output).contains("src/main.rs"));
        Ok(())
    }

    #[tokio::test]
    async fn edit_write_and_delete_mutate_files() -> anyhow::Result<()> {
        let root = TempDir::new()?;

        FileHandler
            .handle(
                invocation(
                    &root,
                    "write",
                    json!({
                        "path": "nested/sample.txt",
                        "content": "alpha beta alpha",
                        "create_parent_dirs": true
                    }),
                )
                .await,
            )
            .await?;

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "nested/sample.txt", "start_line": 1, "end_line": 1 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;

        let edit_output = FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "nested/sample.txt",
                        "old_text": "alpha",
                        "new_text": "omega",
                        "occurrence": 2
                    }),
                    tracker,
                )
                .await,
            )
            .await?;
        let diff = output_text(edit_output);
        assert!(diff.contains("-alpha beta alpha"));
        assert!(diff.contains("+alpha beta omega"));

        let edited = fs::read_to_string(root.path().join("nested/sample.txt")).await?;
        assert_eq!(edited, "alpha beta omega");

        FileHandler
            .handle(
                invocation(
                    &root,
                    "delete",
                    json!({ "path": "nested", "recursive": true }),
                )
                .await,
            )
            .await?;

        assert!(!root.path().join("nested").exists());
        Ok(())
    }

    #[tokio::test]
    async fn write_and_delete_support_safety_options() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("sample.txt"), "alpha").await?;

        let create_err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "write",
                    json!({
                        "path": "sample.txt",
                        "content": "omega",
                        "mode": "create_new"
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("create_new should reject existing files"),
            Err(err) => err,
        };
        assert_eq!(
            create_err,
            FunctionCallError::RespondToModel(format!(
                "refusing to overwrite existing file `{}` with mode=create_new",
                root.path().join("sample.txt").display()
            ))
        );

        let stale_hash_err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "write",
                    json!({
                        "path": "sample.txt",
                        "content": "omega",
                        "expected_hash": "sha1:stale"
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("stale expected_hash should reject writes"),
            Err(err) => err,
        };
        assert_eq!(
            stale_hash_err,
            FunctionCallError::RespondToModel(format!(
                "expected_hash mismatch: expected sha1:stale, actual {}",
                content_hash("alpha")
            ))
        );

        FileHandler
            .handle(
                invocation(
                    &root,
                    "write",
                    json!({
                        "path": "sample.txt",
                        "content": "omega",
                        "mode": "update_existing",
                        "expected_hash": content_hash("alpha")
                    }),
                )
                .await,
            )
            .await?;
        assert_eq!(
            fs::read_to_string(root.path().join("sample.txt")).await?,
            "omega"
        );

        let dry_run_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "delete",
                    json!({
                        "path": "sample.txt",
                        "expected_type": "file",
                        "dry_run": true
                    }),
                )
                .await,
            )
            .await?;
        assert!(output_text(dry_run_output).contains("would delete"));
        assert!(root.path().join("sample.txt").exists());

        let type_err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "delete",
                    json!({
                        "path": "sample.txt",
                        "expected_type": "dir"
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("expected_type should reject mismatches"),
            Err(err) => err,
        };
        assert_eq!(
            type_err,
            FunctionCallError::RespondToModel(format!(
                "refusing to delete `{}` because expected_type does not match",
                root.path().join("sample.txt").display()
            ))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_symlink_target_outside_writable_root() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let outside = TempDir::new()?;
        let outside_file = outside.path().join("target.txt");
        fs::write(&outside_file, "secret").await?;
        std::os::unix::fs::symlink(&outside_file, root.path().join("link.txt"))?;

        let err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "write",
                    json!({
                        "path": "link.txt",
                        "content": "changed"
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("write through symlink should be denied"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("resolves to"),
            "unexpected error: {err}"
        );
        assert_eq!(fs::read_to_string(&outside_file).await?, "secret");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_missing_file_under_symlinked_parent() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let outside = TempDir::new()?;
        let outside_file = outside.path().join("created.txt");
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked"))?;

        let err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "write",
                    json!({
                        "path": "linked/created.txt",
                        "content": "created"
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("write under symlinked parent should be denied"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("resolves to"),
            "unexpected error: {err}"
        );
        assert!(!outside_file.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_rejects_symlink_target_outside_writable_root() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let outside = TempDir::new()?;
        let outside_file = outside.path().join("target.txt");
        fs::write(&outside_file, "secret").await?;
        std::os::unix::fs::symlink(&outside_file, root.path().join("link.txt"))?;

        let err = match FileHandler
            .handle(invocation(&root, "delete", json!({ "path": "link.txt" })).await)
            .await
        {
            Ok(_) => panic!("delete through symlink should be denied"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("resolves to"),
            "unexpected error: {err}"
        );
        assert_eq!(fs::read_to_string(&outside_file).await?, "secret");
        assert!(root.path().join("link.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn edit_ops_apply_multiple_operation_types() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(
            root.path().join("sample.txt"),
            "use beta;\nfn main() {\n    alpha();\n    beta();\n}\n",
        )
        .await?;

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 1, "end_line": 5 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "ops": [
                            {
                                "type": "insert_after",
                                "anchor": "use beta;\n",
                                "text": "use omega;\n"
                            },
                            {
                                "type": "replace",
                                "old_text": "beta",
                                "new_text": "gamma",
                                "occurrence": 2
                            },
                            {
                                "type": "replace_range",
                                "start_line": 4,
                                "end_line": 4,
                                "new_text": "    omega();\n"
                            },
                            {
                                "type": "delete_text",
                                "old_text": "    gamma();\n"
                            }
                        ]
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        let edited = fs::read_to_string(root.path().join("sample.txt")).await?;
        assert_eq!(
            edited,
            "use beta;\nuse omega;\nfn main() {\n    omega();\n}\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn edit_ops_snapshot_resolves_occurrences_against_original_text() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(
            root.path().join("sample.txt"),
            "source: ExecCommandSource::Agent,\nsource: ExecCommandSource::Agent,\n",
        )
        .await?;

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 1, "end_line": 2 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;

        let output = FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "apply_mode": "snapshot",
                        "ops": [
                            {
                                "type": "replace",
                                "old_text": "source: ExecCommandSource::Agent,",
                                "new_text": "auto_review_approved: false,\nsource: ExecCommandSource::Agent,",
                                "occurrence": 2
                            },
                            {
                                "type": "replace",
                                "old_text": "source: ExecCommandSource::Agent,",
                                "new_text": "auto_review_approved: false,\nsource: ExecCommandSource::Agent,",
                                "occurrence": 1
                            }
                        ]
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        assert_eq!(
            fs::read_to_string(root.path().join("sample.txt")).await?,
            "auto_review_approved: false,\nsource: ExecCommandSource::Agent,\nauto_review_approved: false,\nsource: ExecCommandSource::Agent,\n"
        );
        let output = output_text(output);
        assert!(output.contains("Matched ranges:"));
        assert!(output.contains("op 1: bytes"));
        assert!(output.contains("op 2: bytes"));
        Ok(())
    }

    #[tokio::test]
    async fn edit_ops_replace_occurrences_replaces_selected_original_occurrences()
    -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(
            root.path().join("sample.txt"),
            "alpha beta alpha beta alpha",
        )
        .await?;

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 1, "end_line": 1 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "apply_mode": "snapshot",
                        "ops": [
                            {
                                "type": "replace_occurrences",
                                "old_text": "alpha",
                                "new_text": "omega",
                                "occurrences": [1, 3]
                            }
                        ]
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        assert_eq!(
            fs::read_to_string(root.path().join("sample.txt")).await?,
            "omega beta alpha beta omega"
        );
        Ok(())
    }

    #[tokio::test]
    async fn edit_ops_snapshot_rejects_overlapping_ranges_without_writing() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("sample.txt"), "abcdef").await?;

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 1, "end_line": 1 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;

        let err = match FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "apply_mode": "snapshot",
                        "ops": [
                            { "type": "replace", "old_text": "abcde", "new_text": "ABCDE" },
                            { "type": "replace", "old_text": "f", "new_text": "F" },
                            { "type": "replace", "old_text": "cde", "new_text": "CDE" }
                        ]
                    }),
                    tracker,
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("overlapping snapshot ops should fail"),
            Err(err) => err,
        };

        assert_eq!(
            fs::read_to_string(root.path().join("sample.txt")).await?,
            "abcdef"
        );
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "op 3 overlaps op 1; no changes written.\nop 1: bytes 0..5\nop 3: bytes 2..5"
                    .to_string()
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn edit_ops_sequential_warns_when_old_text_is_reused() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("sample.txt"), "foo foo").await?;

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 1, "end_line": 1 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;

        let output = FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "ops": [
                            {
                                "type": "replace",
                                "old_text": "foo",
                                "new_text": "barfoo",
                                "occurrence": 1
                            },
                            {
                                "type": "replace",
                                "old_text": "foo",
                                "new_text": "baz",
                                "occurrence": 1
                            }
                        ]
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        assert_eq!(
            fs::read_to_string(root.path().join("sample.txt")).await?,
            "barbaz foo"
        );
        assert!(output_text(output).starts_with(
            "warning: op 2 occurrence resolved against modified buffer in sequential mode. Use apply_mode=\"snapshot\" for simultaneous replacement."
        ));
        Ok(())
    }

    #[tokio::test]
    async fn edit_ops_are_atomic_on_failure() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("sample.txt"), "alpha\nbeta\n").await?;

        let err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "ops": [
                            {
                                "type": "replace",
                                "old_text": "alpha",
                                "new_text": "omega"
                            },
                            {
                                "type": "delete_text",
                                "old_text": "missing"
                            }
                        ]
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("failed op should abort the whole edit"),
            Err(err) => err,
        };

        assert_eq!(
            err,
            FunctionCallError::RespondToModel("op 2: old_text was not found".to_string())
        );
        let text = fs::read_to_string(root.path().join("sample.txt")).await?;
        assert_eq!(text, "alpha\nbeta\n");
        Ok(())
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_replacement() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("sample.txt"), "alpha beta alpha").await?;

        let err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "alpha",
                        "new_text": "omega"
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("replacement should be ambiguous"),
            Err(err) => err,
        };

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "old_text occurs 2 times; specify occurrence".to_string()
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn edit_requires_observed_lines_and_fresh_hash() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let path = root.path().join("sample.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n").await?;

        let err = match FileHandler
            .handle(
                invocation(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "beta",
                        "new_text": "omega"
                    }),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("edit should require prior read or search"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(format!(
                "refusing to edit `{}` because the target lines have not been read or searched this turn",
                path.display()
            ))
        );

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({
                        "path": "sample.txt",
                        "start_line": 1,
                        "end_line": 1
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        let err = match FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "beta",
                        "new_text": "omega"
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("edit should reject unread lines"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(format!(
                "refusing to edit `{}` because lines 2 have not been read or searched this turn",
                path.display()
            ))
        );

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({
                        "path": "sample.txt",
                        "start_line": 2,
                        "end_line": 2
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        fs::write(&path, "alpha\nBETA\ngamma\n").await?;
        let err = match FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "BETA",
                        "new_text": "omega"
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("edit should reject stale observations"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(format!(
                "refusing to edit `{}` because it changed since it was read or searched: observed {}, actual {}",
                path.display(),
                content_hash("alpha\nbeta\ngamma\n"),
                content_hash("alpha\nBETA\ngamma\n")
            ))
        );

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({
                        "path": "sample.txt",
                        "start_line": 2,
                        "end_line": 2
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "BETA",
                        "new_text": "omega"
                    }),
                    tracker,
                )
                .await,
            )
            .await?;
        assert_eq!(fs::read_to_string(path).await?, "alpha\nomega\ngamma\n");
        Ok(())
    }

    #[tokio::test]
    async fn edit_preserves_observed_lines_when_hash_is_unchanged() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let path = root.path().join("sample.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n").await?;
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 1, "end_line": 3 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "beta",
                        "new_text": "omega"
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "gamma",
                        "new_text": "delta"
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        assert_eq!(fs::read_to_string(path).await?, "alpha\nomega\ndelta\n");
        Ok(())
    }

    #[tokio::test]
    async fn edit_diff_lines_can_be_edited_again_without_reread() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let path = root.path().join("sample.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n").await?;
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 2, "end_line": 2 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "beta",
                        "new_text": "omega"
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "omega",
                        "new_text": "theta"
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        assert_eq!(fs::read_to_string(path).await?, "alpha\ntheta\ngamma\n");
        Ok(())
    }

    #[tokio::test]
    async fn edit_observation_after_edit_still_rejects_external_changes() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let path = root.path().join("sample.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n").await?;
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "read_file",
                    json!({ "path": "sample.txt", "start_line": 2, "end_line": 2 }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "beta",
                        "new_text": "omega"
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        fs::write(&path, "alpha\nOMEGA\ngamma\n").await?;
        let err = match FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "OMEGA",
                        "new_text": "theta"
                    }),
                    tracker,
                )
                .await,
            )
            .await
        {
            Ok(_) => panic!("external change should stale the edit observation"),
            Err(err) => err,
        };

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(format!(
                "refusing to edit `{}` because it changed since it was read or searched: observed {}, actual {}",
                path.display(),
                content_hash("alpha\nomega\ngamma\n"),
                content_hash("alpha\nOMEGA\ngamma\n")
            ))
        );
        Ok(())
    }

    #[tokio::test]
    async fn write_observes_the_written_file_for_followup_edit() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let path = root.path().join("sample.txt");
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "write",
                    json!({
                        "path": "sample.txt",
                        "content": "alpha\nbeta\ngamma\n"
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "gamma",
                        "new_text": "delta"
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        assert_eq!(fs::read_to_string(path).await?, "alpha\nbeta\ndelta\n");
        Ok(())
    }

    #[tokio::test]
    async fn grep_observation_allows_matching_line_edit() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        let path = root.path().join("sample.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n").await?;
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "grep_file",
                    json!({
                        "pattern": "beta"
                    }),
                    tracker.clone(),
                )
                .await,
            )
            .await?;
        FileHandler
            .handle(
                invocation_with_tracker(
                    &root,
                    "edit",
                    json!({
                        "path": "sample.txt",
                        "old_text": "beta",
                        "new_text": "omega"
                    }),
                    tracker,
                )
                .await,
            )
            .await?;

        assert_eq!(fs::read_to_string(path).await?, "alpha\nomega\ngamma\n");
        Ok(())
    }

    #[tokio::test]
    async fn delete_rejects_directory_without_recursive() -> anyhow::Result<()> {
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("nested")).await?;

        let err = match FileHandler
            .handle(invocation(&root, "delete", json!({ "path": "nested" })).await)
            .await
        {
            Ok(_) => panic!("directory delete should require recursive=true"),
            Err(err) => err,
        };

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "refusing to delete directory without recursive=true".to_string()
            )
        );
        Ok(())
    }
}
