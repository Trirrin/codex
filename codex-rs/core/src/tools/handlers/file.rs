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
use similar::TextDiff;
use std::collections::HashMap;
use tokio::fs;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
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
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    occurrence: Option<usize>,
    #[serde(default)]
    ops: Vec<EditOperation>,
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
                    cmd: "read_file".to_string(),
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
                    let output = if args.include_line_numbers {
                        lines
                            .into_iter()
                            .map(|(line_number, line)| format!("{line_number}:{line}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        lines
                            .into_iter()
                            .map(|(_, line)| line.to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    let output = truncate(output, args.max_output_tokens);
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
                let operations = edit_operations(args)?;
                let text = fs::read_to_string(&path).await.map_err(io_error)?;
                let edited = apply_edit_operations(&text, &operations)?;
                let diff = unified_update_diff(&path, &text, &edited);
                let changes = single_update_change(&path, &text, &edited);
                emit_patch_begin(session.as_ref(), turn.as_ref(), &call_id, &changes).await;
                let result = fs::write(&path, edited)
                    .await
                    .map_err(io_error)
                    .map(|_| FunctionToolOutput::from_text(diff, Some(true)));
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
                let changes = single_write_change(&path, previous.as_deref(), &args.content);
                emit_patch_begin(session.as_ref(), turn.as_ref(), &call_id, &changes).await;
                let result = fs::write(&path, args.content)
                    .await
                    .map_err(io_error)
                    .map(|_| {
                        FunctionToolOutput::from_text(
                            format!("wrote {}", path.display()),
                            Some(true),
                        )
                    });
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
) -> Result<(), FunctionCallError> {
    let mut files = Vec::new();
    collect_files(turn, root, options.filters, usize::MAX, &mut files).await?;
    for file in files {
        let Ok(text) = fs::read_to_string(&file).await else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if !options.regex.is_match(line) {
                continue;
            }
            let start = index.saturating_sub(options.context);
            let end = (index + options.context + 1).min(lines.len());
            for (line_index, line) in lines[start..end].iter().enumerate() {
                matches.push(format!(
                    "{}:{}:{}",
                    file.display(),
                    start + line_index + 1,
                    line
                ));
            }
            if matches.len() >= options.limit {
                return Ok(());
            }
        }
    }
    Ok(())
}

async fn collect_grep_structured_matches(
    turn: &crate::session::turn_context::TurnContext,
    root: &Path,
    options: &GrepCollectOptions<'_>,
    matches: &mut Vec<GrepMatch>,
) -> Result<(), FunctionCallError> {
    let mut files = Vec::new();
    collect_files(turn, root, options.filters, usize::MAX, &mut files).await?;
    for file in files {
        let Ok(text) = fs::read_to_string(&file).await else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if !options.regex.is_match(line) {
                continue;
            }
            let before_start = index.saturating_sub(options.context);
            let context_before = lines[before_start..index]
                .iter()
                .enumerate()
                .map(|(context_index, line)| GrepContextLine {
                    line: before_start + context_index + 1,
                    text: (*line).to_string(),
                })
                .collect();
            let after_end = (index + options.context + 1).min(lines.len());
            let context_after = lines[index + 1..after_end]
                .iter()
                .enumerate()
                .map(|(context_index, line)| GrepContextLine {
                    line: index + context_index + 2,
                    text: (*line).to_string(),
                })
                .collect();
            matches.push(GrepMatch {
                path: file.display().to_string(),
                line: index + 1,
                text: (*line).to_string(),
                context_before,
                context_after,
            });
            if matches.len() >= options.limit {
                return Ok(());
            }
        }
    }
    Ok(())
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

fn edit_operations(args: EditArgs) -> Result<Vec<EditOperation>, FunctionCallError> {
    if !args.ops.is_empty() {
        if args.old_text.is_some() || args.new_text.is_some() || args.occurrence.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "use either ops or legacy old_text/new_text fields, not both".to_string(),
            ));
        }
        return Ok(args.ops);
    }

    let old_text = args.old_text.ok_or_else(|| {
        FunctionCallError::RespondToModel("old_text is required when ops is omitted".to_string())
    })?;
    let new_text = args.new_text.ok_or_else(|| {
        FunctionCallError::RespondToModel("new_text is required when ops is omitted".to_string())
    })?;
    Ok(vec![EditOperation::Replace {
        old_text,
        new_text,
        occurrence: args.occurrence,
    }])
}

fn apply_edit_operations(
    text: &str,
    operations: &[EditOperation],
) -> Result<String, FunctionCallError> {
    let mut edited = text.to_string();
    for (index, operation) in operations.iter().enumerate() {
        edited = apply_edit_operation(&edited, operation).map_err(|err| match err {
            FunctionCallError::RespondToModel(message) if operations.len() > 1 => {
                FunctionCallError::RespondToModel(format!("op {}: {message}", index + 1))
            }
            err => err,
        })?;
    }
    Ok(edited)
}

fn apply_edit_operation(
    text: &str,
    operation: &EditOperation,
) -> Result<String, FunctionCallError> {
    match operation {
        EditOperation::Replace {
            old_text,
            new_text,
            occurrence,
        } => {
            let occurrence = resolve_occurrence(text, old_text, *occurrence, "old_text")?;
            Ok(replace_occurrence(text, old_text, new_text, occurrence))
        }
        EditOperation::InsertBefore {
            anchor,
            text: insert_text,
            occurrence,
        } => {
            let occurrence = resolve_occurrence(text, anchor, *occurrence, "anchor")?;
            Ok(replace_occurrence(
                text,
                anchor,
                &format!("{insert_text}{anchor}"),
                occurrence,
            ))
        }
        EditOperation::InsertAfter {
            anchor,
            text: insert_text,
            occurrence,
        } => {
            let occurrence = resolve_occurrence(text, anchor, *occurrence, "anchor")?;
            Ok(replace_occurrence(
                text,
                anchor,
                &format!("{anchor}{insert_text}"),
                occurrence,
            ))
        }
        EditOperation::ReplaceRange {
            start_line,
            end_line,
            new_text,
        } => replace_line_range(text, *start_line, *end_line, new_text),
        EditOperation::DeleteRange {
            start_line,
            end_line,
        } => replace_line_range(text, *start_line, *end_line, ""),
        EditOperation::DeleteText {
            old_text,
            occurrence,
        } => {
            let occurrence = resolve_occurrence(text, old_text, *occurrence, "old_text")?;
            Ok(replace_occurrence(text, old_text, "", occurrence))
        }
    }
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

fn replace_line_range(
    text: &str,
    start_line: usize,
    end_line: usize,
    new_text: &str,
) -> Result<String, FunctionCallError> {
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

    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    if end_line > lines.len() {
        return Err(FunctionCallError::RespondToModel(format!(
            "line range {start_line}-{end_line} is out of range; file has {} lines",
            lines.len()
        )));
    }

    let mut edited = String::new();
    for line in &lines[..start_line - 1] {
        edited.push_str(line);
    }
    edited.push_str(new_text);
    for line in &lines[end_line..] {
        edited.push_str(line);
    }
    Ok(edited)
}

fn replace_occurrence(text: &str, old_text: &str, new_text: &str, occurrence: usize) -> String {
    let mut start = 0;
    for _ in 1..occurrence {
        let Some(index) = text[start..].find(old_text) else {
            return text.to_string();
        };
        start += index + old_text.len();
    }
    let Some(index) = text[start..].find(old_text) else {
        return text.to_string();
    };
    let absolute = start + index;
    format!(
        "{}{}{}",
        &text[..absolute],
        new_text,
        &text[absolute + old_text.len()..]
    )
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
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
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

        let edit_output = FileHandler
            .handle(
                invocation(
                    &root,
                    "edit",
                    json!({
                        "path": "nested/sample.txt",
                        "old_text": "alpha",
                        "new_text": "omega",
                        "occurrence": 2
                    }),
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

        FileHandler
            .handle(
                invocation(
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
