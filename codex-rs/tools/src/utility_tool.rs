use crate::JsonSchema;
use crate::ResponsesApiTool;
use crate::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub fn create_read_file_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "path".to_string(),
            JsonSchema::string(Some("Path to the file to read.".to_string())),
        ),
        (
            "offset".to_string(),
            JsonSchema::number(Some(
                "Zero-based line offset to start reading from.".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("Maximum number of lines to return.".to_string())),
        ),
        (
            "start_line".to_string(),
            JsonSchema::number(Some(
                "One-based start line. When set, this is used instead of offset.".to_string(),
            )),
        ),
        (
            "end_line".to_string(),
            JsonSchema::number(Some(
                "One-based inclusive end line. Requires start_line.".to_string(),
            )),
        ),
        (
            "include_line_numbers".to_string(),
            JsonSchema::boolean(Some(
                "Whether to prefix each returned line with its one-based line number.".to_string(),
            )),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::number(Some(
                "Maximum number of tokens to return. Excess output will be truncated.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "read_file".to_string(),
        description: "Reads a local text file with optional line pagination.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn output_format_schema() -> JsonSchema {
    JsonSchema::string_enum(
        vec![json!("text"), json!("json")],
        Some("Output format. Defaults to text for backward compatibility.".to_string()),
    )
}

pub fn create_search_file_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            JsonSchema::string(Some("Path query to search for.".to_string())),
        ),
        (
            "roots".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Root directories to search; defaults to the turn cwd.".to_string()),
            ),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("Maximum number of matches to return.".to_string())),
        ),
        (
            "include".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Path substrings that must be present. This is not glob matching; use include_globs for glob patterns.".to_string()),
            ),
        ),
        (
            "exclude".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Path substrings to exclude.".to_string()),
            ),
        ),
        (
            "include_globs".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Glob patterns that matching paths must satisfy.".to_string()),
            ),
        ),
        (
            "exclude_globs".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Glob patterns that matching paths must not satisfy.".to_string()),
            ),
        ),
        ("output_format".to_string(), output_format_schema()),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "search_file".to_string(),
        description: "Searches local file paths under one or more roots.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_grep_file_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "pattern".to_string(),
            JsonSchema::string(Some("Text or regex pattern to search for.".to_string())),
        ),
        (
            "roots".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Root directories or files to search; defaults to the turn cwd.".to_string()),
            ),
        ),
        (
            "include".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Path substrings that must be present. This is not glob matching; use include_globs for glob patterns.".to_string()),
            ),
        ),
        (
            "exclude".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Path substrings to exclude.".to_string()),
            ),
        ),
        (
            "include_globs".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Glob patterns that matching paths must satisfy.".to_string()),
            ),
        ),
        (
            "exclude_globs".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Glob patterns that matching paths must not satisfy.".to_string()),
            ),
        ),
        (
            "case_sensitive".to_string(),
            JsonSchema::boolean(Some(
                "Whether matching is case-sensitive. Defaults to true.".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("Maximum number of matches to return.".to_string())),
        ),
        (
            "context".to_string(),
            JsonSchema::number(Some(
                "Number of surrounding lines to include for each match.".to_string(),
            )),
        ),
        ("output_format".to_string(), output_format_schema()),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "grep_file".to_string(),
        description: "Searches local text file contents.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["pattern".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_glob_file_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "pattern".to_string(),
            JsonSchema::string(Some("Glob pattern to match.".to_string())),
        ),
        (
            "root".to_string(),
            JsonSchema::string(Some(
                "Root directory for relative patterns; defaults to the turn cwd.".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("Maximum number of matches to return.".to_string())),
        ),
        (
            "relative_only".to_string(),
            JsonSchema::boolean(Some(
                "Whether text output should return paths relative to root when possible."
                    .to_string(),
            )),
        ),
        (
            "entry_type".to_string(),
            JsonSchema::string_enum(
                vec![json!("file"), json!("dir"), json!("any")],
                Some(
                    "Entry type to return. Defaults to file for backward compatibility."
                        .to_string(),
                ),
            ),
        ),
        ("output_format".to_string(), output_format_schema()),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "glob_file".to_string(),
        description: "Finds local paths matching a glob pattern.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["pattern".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_list_dir_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "dir_path".to_string(),
            JsonSchema::string(Some("Absolute path to the directory to list.".to_string())),
        ),
        (
            "offset".to_string(),
            JsonSchema::number(Some(
                "The entry number to start listing from. Must be 1 or greater.".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("The maximum number of entries to return.".to_string())),
        ),
        (
            "depth".to_string(),
            JsonSchema::number(Some(
                "The maximum directory depth to traverse. Must be 1 or greater.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "list_dir".to_string(),
        description:
            "Lists entries in a local directory with 1-indexed entry numbers and simple type labels."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["dir_path".to_string()]), Some(false.into())),
        output_schema: None,
    })
}

fn edit_operation_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "type".to_string(),
                JsonSchema::string_enum(
                    vec![
                        json!("replace"),
                        json!("insert_before"),
                        json!("insert_after"),
                        json!("replace_range"),
                        json!("delete_range"),
                        json!("delete_text"),
                    ],
                    Some(
                        "Operation to apply in order. replace/delete_text use old_text; insert_* use anchor and text; *_range use one-based inclusive start_line and end_line in the current text for that operation."
                            .to_string(),
                    ),
                ),
            ),
            (
                "old_text".to_string(),
                JsonSchema::string(Some("Existing text to replace or delete.".to_string())),
            ),
            (
                "new_text".to_string(),
                JsonSchema::string(Some("Replacement text for replace or replace_range.".to_string())),
            ),
            (
                "anchor".to_string(),
                JsonSchema::string(Some("Existing text to insert before or after.".to_string())),
            ),
            (
                "text".to_string(),
                JsonSchema::string(Some("Text to insert.".to_string())),
            ),
            (
                "occurrence".to_string(),
                JsonSchema::number(Some(
                    "One-based occurrence to edit. Omit only when old_text or anchor occurs exactly once."
                        .to_string(),
                )),
            ),
            (
                "start_line".to_string(),
                JsonSchema::number(Some(
                    "One-based first line for range operations.".to_string(),
                )),
            ),
            (
                "end_line".to_string(),
                JsonSchema::number(Some(
                    "One-based inclusive last line for range operations.".to_string(),
                )),
            ),
        ]),
        Some(vec!["type".to_string()]),
        Some(false.into()),
    )
}

fn text_replacement_properties() -> BTreeMap<String, JsonSchema> {
    BTreeMap::from([
        (
            "path".to_string(),
            JsonSchema::string(Some("Path to the file to edit.".to_string())),
        ),
        (
            "ops".to_string(),
            JsonSchema::array(
                edit_operation_schema(),
                Some(
                    "Atomic edit operations to apply in order. Use this for inserts, deletions, ranges, or multiple edits."
                        .to_string(),
                ),
            ),
        ),
        (
            "old_text".to_string(),
            JsonSchema::string(Some(
                "Legacy single-replacement text. Prefer ops for new calls.".to_string(),
            )),
        ),
        (
            "new_text".to_string(),
            JsonSchema::string(Some(
                "Legacy single-replacement text. Prefer ops for new calls.".to_string(),
            )),
        ),
        (
            "occurrence".to_string(),
            JsonSchema::number(Some(
                "Legacy one-based occurrence to replace. Omit only when old_text occurs exactly once."
                    .to_string(),
            )),
        ),
    ])
}

pub fn create_edit_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "edit".to_string(),
        description:
            "Edits a single local file. Prefer ops for atomic inserts, deletes, ranges, or multiple edits; legacy old_text/new_text remains supported."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            text_replacement_properties(),
            Some(vec!["path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_write_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "path".to_string(),
            JsonSchema::string(Some("Path to write.".to_string())),
        ),
        (
            "content".to_string(),
            JsonSchema::string(Some("Complete file content.".to_string())),
        ),
        (
            "create_parent_dirs".to_string(),
            JsonSchema::boolean(Some(
                "Whether to create missing parent directories. Defaults to false.".to_string(),
            )),
        ),
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![json!("overwrite"), json!("create_new"), json!("update_existing")],
                Some("Write mode. Defaults to overwrite for backward compatibility.".to_string()),
            ),
        ),
        (
            "expected_hash".to_string(),
            JsonSchema::string(Some(
                "Optional sha1:<hex> hash of the current file content. The write is rejected if it does not match.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "write".to_string(),
        description: "Writes complete content to a local file.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["path".to_string(), "content".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_delete_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "path".to_string(),
            JsonSchema::string(Some("Path to delete.".to_string())),
        ),
        (
            "recursive".to_string(),
            JsonSchema::boolean(Some(
                "Required and true to delete a directory recursively.".to_string(),
            )),
        ),
        (
            "expected_type".to_string(),
            JsonSchema::string_enum(
                vec![json!("file"), json!("dir")],
                Some(
                    "Expected path type. The delete is rejected if the actual type does not match."
                        .to_string(),
                ),
            ),
        ),
        (
            "dry_run".to_string(),
            JsonSchema::boolean(Some(
                "Report what would be deleted without deleting anything.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "delete".to_string(),
        description: "Deletes a local file, or a directory when recursive=true.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_test_sync_tool() -> ToolSpec {
    let barrier_properties = BTreeMap::from([
        (
            "id".to_string(),
            JsonSchema::string(Some(
                "Identifier shared by concurrent calls that should rendezvous".to_string(),
            )),
        ),
        (
            "participants".to_string(),
            JsonSchema::number(Some(
                "Number of tool calls that must arrive before the barrier opens".to_string(),
            )),
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::number(Some(
                "Maximum time in milliseconds to wait at the barrier".to_string(),
            )),
        ),
    ]);

    let properties = BTreeMap::from([
        (
            "sleep_before_ms".to_string(),
            JsonSchema::number(Some(
                "Optional delay in milliseconds before any other action".to_string(),
            )),
        ),
        (
            "sleep_after_ms".to_string(),
            JsonSchema::number(Some(
                "Optional delay in milliseconds after completing the barrier".to_string(),
            )),
        ),
        (
            "barrier".to_string(),
            JsonSchema::object(
                barrier_properties,
                Some(vec!["id".to_string(), "participants".to_string()]),
                Some(false.into()),
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "test_sync_tool".to_string(),
        description: "Internal synchronization helper used by Codex integration tests.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "utility_tool_tests.rs"]
mod tests;
