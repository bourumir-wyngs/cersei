//! Exact-position and sed-backed file editing with one-step session-local revert.

use super::*;
use crate::file_history::{unified_diff, FileHistory};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Debug, Clone)]
struct LastEditSnapshot {
    file_path: PathBuf,
    content: Vec<u8>,
}

static LAST_EDIT_SNAPSHOT_REGISTRY: Lazy<dashmap::DashMap<String, LastEditSnapshot>> =
    Lazy::new(dashmap::DashMap::new);

pub struct EditTool;
pub struct SedTool;
pub struct RevertTool;

/// Public alias preserved for downstream imports.
pub type FileEditTool = EditTool;

#[derive(Debug, Clone, Deserialize)]
pub struct EditRequest {
    pub file_path: String,
    pub base_version: String,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextEdit {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub expected_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditSuccess {
    pub ok: bool,
    pub file_path: String,
    pub old_version: String,
    pub new_version: String,
    pub applied_edits: usize,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditFailure {
    pub ok: bool,
    pub file_path: String,
    pub code: &'static str,
    pub message: String,
    pub edit_index: Option<usize>,
    pub current_version: Option<String>,
    pub actual_text: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct LineInfo {
    start: usize,
    end: usize,
    next_start: usize,
}

#[derive(Debug, Clone)]
struct DocumentIndex {
    lines: Vec<LineInfo>,
}

#[derive(Debug, Clone)]
struct ResolvedEdit {
    start: usize,
    end: usize,
    expected_text: String,
    new_text: String,
    source_index: usize,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Apply exact, byte-precise edits to an existing UTF-8 file using 1-based line/column \
         positions. Every request must provide a strict base_version hash plus the exact \
         expected_text for each edit. Returns structured JSON."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file relative to the current workspace root. Absolute paths and `..` segments are not allowed."
                },
                "base_version": {
                    "type": "string",
                    "description": "Exact file version hash in the form `blake3:<hex>`. Requests fail if the file has changed."
                },
                "edits": {
                    "type": "array",
                    "description": "Edits to apply. Positions are 1-based. Columns count Unicode scalar values within a line.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "start_line": { "type": "integer", "minimum": 1 },
                            "start_column": { "type": "integer", "minimum": 1 },
                            "end_line": { "type": "integer", "minimum": 1 },
                            "end_column": { "type": "integer", "minimum": 1 },
                            "expected_text": { "type": "string" },
                            "new_text": { "type": "string" }
                        },
                        "required": [
                            "start_line",
                            "start_column",
                            "end_line",
                            "end_column",
                            "expected_text",
                            "new_text"
                        ]
                    }
                }
            },
            "required": ["file_path", "base_version", "edits"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let raw_input = input.clone();
        let req: EditRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => {
                let failure =
                    invalid_request_failure(&raw_input, format!("Invalid input: {}", err));
                return ToolResult::error(serialize_failure(&failure));
            }
        };

        let path = match resolve_edit_path(ctx, &req.file_path) {
            Ok(path) => path,
            Err(failure) => return ToolResult::error(serialize_failure(&failure)),
        };

        match execute_edit_inner(req, &path, Some(ctx)).await {
            Ok(payload) => ToolResult::success(payload),
            Err(failure) => ToolResult::error(serialize_failure(&failure)),
        }
    }
}

#[async_trait]
impl Tool for SedTool {
    fn name(&self) -> &str {
        "Sed"
    }

    fn description(&self) -> &str {
        "Apply a sed script to a file using sed-rs (Rust regex / ERE). NOTE: unlike standard GNU \
         sed, characters like `{`, `}`, `(`, `)`, `+`, and `?` are special by default and must \
         be escaped (e.g., `\\{`) to match literals. The file is checkpointed before the write, \
         the result is written back to disk, and the tool returns a unified diff plus the \
         reminder \"use 'revert' command if wrong\"."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file relative to the current workspace root. Absolute paths and `..` segments are not allowed."
                },
                "script": {
                    "type": "string",
                    "description": "Sed script using Extended Regular Expressions (ERE), e.g. `s/foo/bar/g`. Escape {, }, (, ), +, ? to match as literals!"
                },
                "quiet": {
                    "type": "boolean",
                    "description": "Suppress automatic printing of the pattern space (`-n` behavior)",
                    "default": false
                },
                "null_data": {
                    "type": "boolean",
                    "description": "Use NUL as the record delimiter (`-z` behavior)",
                    "default": false
                }
            },
            "required": ["file_path", "script"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            file_path: String,
            script: String,
            #[serde(default)]
            quiet: bool,
            #[serde(default)]
            null_data: bool,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = match resolve_existing_workspace_path(ctx, &input.file_path) {
            Ok(path) => path,
            Err(err) => return err,
        };

        let original_bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolResult::error(format!("Failed to read file: {}", e)),
        };
        let original_text = String::from_utf8_lossy(&original_bytes).into_owned();

        let mut sed = match sed_rs::Sed::new(&input.script) {
            Ok(sed) => sed,
            Err(e) => return ToolResult::error(format!("Invalid sed script: {}\n\nNOTE: Unlike standard GNU sed, this tool uses Rust regex (Extended Regular Expressions). Characters like `{{`, `}}`, `(`, `)`, `+`, and `?` are special by default and must be escaped (e.g., `\\{{`) to match literals in code. To use capture groups, do NOT escape the parentheses: use `(...)` instead of `\\(...\\)`.", e)),
        };
        sed.quiet(input.quiet).null_data(input.null_data);

        let updated_text = match sed.eval_bytes(&original_bytes) {
            Ok(output) => output,
            Err(e) => return ToolResult::error(format!("Failed to run sed script: {}", e)),
        };

        if updated_text.as_bytes() == original_bytes.as_slice() {
            return ToolResult::success(format!(
                "sed script produced no changes in {}.",
                path.display()
            ));
        }

        let snapshot = LastEditSnapshot {
            file_path: path.clone(),
            content: original_bytes.clone(),
        };
        let previous_snapshot =
            LAST_EDIT_SNAPSHOT_REGISTRY.insert(ctx.session_id.clone(), snapshot);

        if let Err(e) = write_bytes(&path, updated_text.as_bytes()).await {
            restore_previous_snapshot(&ctx.session_id, previous_snapshot);
            return ToolResult::error(format!("Failed to write file: {}", e));
        }

        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.record_change(&path, Some(&original_text), &updated_text, "edit");
        }

        let diff = unified_diff(
            &original_text,
            &updated_text,
            &format!("{} (before)", path.display()),
            &format!("{} (after)", path.display()),
        );

        ToolResult::success(format!(
            "Applied sed script to {}.\n{}\n\nuse 'revert' command if wrong",
            path.display(),
            diff.trim_end()
        ))
    }
}

#[async_trait]
impl Tool for RevertTool {
    fn name(&self) -> &str {
        "Revert"
    }

    fn description(&self) -> &str {
        "Restore the previous checkpoint from the most recent successful `Edit` or `Sed` edit in \
         this session. Only one checkpoint is retained."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Optional safety check. If provided, it must be a workspace-relative path matching the file from the most recent edit."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize, Default)]
        struct Input {
            file_path: Option<String>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let snapshot = match LAST_EDIT_SNAPSHOT_REGISTRY.get(&ctx.session_id) {
            Some(entry) => entry.clone(),
            None => {
                return ToolResult::error(
                    "No edit snapshot is available to revert. Run `Edit` or `Sed` first.",
                )
            }
        };

        if let Some(requested) = input.file_path.as_ref() {
            let requested_path = match resolve_workspace_path(ctx, requested, false) {
                Ok(path) => path,
                Err(err) => return err,
            };
            if requested_path != snapshot.file_path {
                return ToolResult::error(format!(
                    "The last edit snapshot is for {}, not {}.",
                    snapshot.file_path.display(),
                    requested
                ));
            }
        }

        let current_bytes = tokio::fs::read(&snapshot.file_path)
            .await
            .unwrap_or_default();
        let current_text = String::from_utf8_lossy(&current_bytes).into_owned();
        let restored_text = String::from_utf8_lossy(&snapshot.content).into_owned();

        if let Err(e) = write_bytes(&snapshot.file_path, &snapshot.content).await {
            return ToolResult::error(format!("Failed to write file: {}", e));
        }

        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.record_change(&snapshot.file_path, Some(&current_text), &restored_text, "revert");
        }

        LAST_EDIT_SNAPSHOT_REGISTRY.remove(&ctx.session_id);

        let diff = unified_diff(
            &current_text,
            &restored_text,
            &format!("{} (current)", snapshot.file_path.display()),
            &format!("{} (reverted)", snapshot.file_path.display()),
        );

        ToolResult::success(format!(
            "Reverted {} to the previous edit snapshot.\n{}",
            snapshot.file_path.display(),
            diff.trim_end()
        ))
    }
}

pub async fn execute_edit(req: EditRequest) -> std::result::Result<String, String> {
    let path = PathBuf::from(&req.file_path);
    execute_edit_inner(req, &path, None)
        .await
        .map_err(|failure| serialize_failure(&failure))
}

async fn execute_edit_inner(
    req: EditRequest,
    path: &Path,
    ctx: Option<&ToolContext>,
) -> std::result::Result<String, EditFailure> {
    let (bytes, text, version) = read_file(path, &req.file_path).await?;

    if version != req.base_version {
        return Err(failure(
            &req.file_path,
            "VERSION_MISMATCH",
            format!(
                "Base version mismatch: expected {}, found {}.",
                req.base_version, version
            ),
            None,
            Some(version),
            None,
        ));
    }

    let index = build_index(&text);
    let resolved = resolve_edits(&text, &index, &req.edits, &req.file_path, &version)?;
    let new_text = apply_edits(&text, &resolved);

    if new_text != text {
        write_atomic(path, &new_text, &req.file_path).await?;

        if let Some(ctx) = ctx {
            if let Some(history) = ctx.extensions.get::<FileHistory>() {
                history.record_change(&path.to_path_buf(), Some(&text), &new_text, "edit");
            }
            LAST_EDIT_SNAPSHOT_REGISTRY.insert(
                ctx.session_id.clone(),
                LastEditSnapshot {
                    file_path: path.to_path_buf(),
                    content: bytes,
                },
            );
        }
    }

    let new_version = compute_version(new_text.as_bytes());
    let diff = make_diff(&text, &new_text);

    Ok(serialize_success(&EditSuccess {
        ok: true,
        file_path: req.file_path,
        old_version: version,
        new_version,
        applied_edits: resolved.len(),
        diff,
    }))
}

/// Compute a version hash from the exact file bytes.
fn compute_version(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

async fn read_file(
    path: &Path,
    file_path: &str,
) -> std::result::Result<(Vec<u8>, String, String), EditFailure> {
    let bytes = tokio::fs::read(path).await.map_err(|_| {
        failure(
            file_path,
            "FILE_NOT_FOUND",
            "Cannot read file",
            None,
            None,
            None,
        )
    })?;

    let version = compute_version(&bytes);
    let text = String::from_utf8(bytes.clone()).map_err(|_| {
        failure(
            file_path,
            "FILE_NOT_UTF8",
            "Invalid UTF-8",
            None,
            Some(version.clone()),
            None,
        )
    })?;

    Ok((bytes, text, version))
}

fn build_index(text: &str) -> DocumentIndex {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return DocumentIndex {
            lines: vec![LineInfo {
                start: 0,
                end: 0,
                next_start: 0,
            }],
        };
    }

    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            lines.push(LineInfo {
                start,
                end,
                next_start: i + 1,
            });
            start = i + 1;
        }
        i += 1;
    }

    if start < bytes.len() {
        lines.push(LineInfo {
            start,
            end: bytes.len(),
            next_start: bytes.len(),
        });
    }

    DocumentIndex { lines }
}

fn position_to_offset(
    text: &str,
    index: &DocumentIndex,
    line: usize,
    column: usize,
) -> std::result::Result<usize, String> {
    if line == 0 || column == 0 {
        return Err("Lines and columns are 1-based.".into());
    }

    if line == index.lines.len() + 1 {
        return if column == 1 {
            Ok(text.len())
        } else {
            Err(format!(
                "Line {} refers to EOF; only column 1 is valid there.",
                line
            ))
        };
    }

    let info = index.lines.get(line - 1).ok_or_else(|| {
        format!(
            "Line {} is out of range for a document with {} line(s).",
            line,
            index.lines.len()
        )
    })?;

    debug_assert!(info.end <= info.next_start);
    let line_text = &text[info.start..info.end];
    let char_count = line_text.chars().count();

    if column > char_count + 1 {
        return Err(format!(
            "Column {} is out of range for line {}; max column is {}.",
            column,
            line,
            char_count + 1
        ));
    }

    if column == char_count + 1 {
        return Ok(info.end);
    }

    let char_offset = column - 1;
    let byte_offset = if char_offset == 0 {
        0
    } else {
        line_text
            .char_indices()
            .nth(char_offset)
            .map(|(offset, _)| offset)
            .unwrap_or(info.end - info.start)
    };

    Ok(info.start + byte_offset)
}

fn resolve_edits(
    text: &str,
    index: &DocumentIndex,
    edits: &[TextEdit],
    file_path: &str,
    current_version: &str,
) -> std::result::Result<Vec<ResolvedEdit>, EditFailure> {
    let mut resolved = Vec::with_capacity(edits.len());

    for (edit_index, edit) in edits.iter().enumerate() {
        let start = position_to_offset(text, index, edit.start_line, edit.start_column).map_err(
            |message| {
                failure(
                    file_path,
                    "INVALID_POSITION",
                    format!("Invalid start position for edit {}: {}", edit_index, message),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    None,
                )
            },
        )?;

        let end = position_to_offset(text, index, edit.end_line, edit.end_column).map_err(
            |message| {
                failure(
                    file_path,
                    "INVALID_POSITION",
                    format!("Invalid end position for edit {}: {}", edit_index, message),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    None,
                )
            },
        )?;

        if start > end {
            return Err(failure(
                file_path,
                "INVALID_RANGE",
                format!("Edit {} start position is after its end position.", edit_index),
                Some(edit_index),
                Some(current_version.to_string()),
                None,
            ));
        }

        let actual = &text[start..end];
        if actual != edit.expected_text {
            return Err(failure(
                file_path,
                "EXPECTED_TEXT_MISMATCH",
                format!(
                    "Edit {} expected text does not match the current file content.",
                    edit_index
                ),
                Some(edit_index),
                Some(current_version.to_string()),
                Some(actual.to_string()),
            ));
        }

        resolved.push(ResolvedEdit {
            start,
            end,
            expected_text: edit.expected_text.clone(),
            new_text: edit.new_text.clone(),
            source_index: edit_index,
        });
    }

    resolved.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.end.cmp(&b.end))
            .then(a.source_index.cmp(&b.source_index))
    });

    for pair in resolved.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if current.start < previous.end || current.start == previous.start {
            return Err(failure(
                file_path,
                "OVERLAPPING_EDITS",
                format!(
                    "Edit {} overlaps or shares a start position with edit {}.",
                    current.source_index, previous.source_index
                ),
                Some(current.source_index),
                Some(current_version.to_string()),
                None,
            ));
        }
    }

    Ok(resolved)
}

fn apply_edits(content: &str, edits: &[ResolvedEdit]) -> String {
    let mut result = content.to_string();

    for edit in edits.iter().rev() {
        debug_assert_eq!(&result[edit.start..edit.end], edit.expected_text.as_str());
        result.replace_range(edit.start..edit.end, &edit.new_text);
    }

    result
}

async fn write_atomic(
    path: &Path,
    content: &str,
    file_path: &str,
) -> std::result::Result<(), EditFailure> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut tmp = NamedTempFile::new_in(parent)
        .map_err(|err| write_failure(file_path, format!("Cannot create temp file: {}", err)))?;

    if let Ok(metadata) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(tmp.path(), metadata.permissions());
    }

    tmp.write_all(content.as_bytes())
        .map_err(|err| write_failure(file_path, format!("Cannot write temp file: {}", err)))?;
    tmp.flush()
        .map_err(|err| write_failure(file_path, format!("Cannot flush temp file: {}", err)))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|err| write_failure(file_path, format!("Cannot sync temp file: {}", err)))?;
    tmp.persist(path)
        .map_err(|err| write_failure(file_path, format!("Cannot persist temp file: {}", err.error)))?;

    Ok(())
}

fn make_diff(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new).unified_diff().to_string()
}

fn write_failure(file_path: &str, message: String) -> EditFailure {
    failure(file_path, "WRITE_FAILED", message, None, None, None)
}

fn resolve_edit_path(
    ctx: &ToolContext,
    input: &str,
) -> std::result::Result<PathBuf, EditFailure> {
    let candidate = Path::new(input);
    if candidate.is_absolute() {
        return Err(failure(
            input,
            "INVALID_PATH",
            "Absolute paths are not allowed; use a workspace-relative path.",
            None,
            None,
            None,
        ));
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(failure(
            input,
            "INVALID_PATH",
            "Path traversal with `..` is not allowed.",
            None,
            None,
            None,
        ));
    }

    let path = ctx.working_dir.join(candidate);
    if !path.exists() {
        return Err(failure(
            input,
            "FILE_NOT_FOUND",
            "Cannot read file",
            None,
            None,
            None,
        ));
    }

    Ok(path)
}

fn failure(
    file_path: &str,
    code: &'static str,
    message: impl Into<String>,
    edit_index: Option<usize>,
    current_version: Option<String>,
    actual_text: Option<String>,
) -> EditFailure {
    EditFailure {
        ok: false,
        file_path: file_path.to_string(),
        code,
        message: message.into(),
        edit_index,
        current_version,
        actual_text,
    }
}

fn invalid_request_failure(input: &Value, message: String) -> EditFailure {
    let file_path = input
        .get("file_path")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    failure(&file_path, "INVALID_REQUEST", message, None, None, None)
}

fn serialize_success(success: &EditSuccess) -> String {
    serde_json::to_string(success).unwrap_or_else(|_| {
        "{\"ok\":false,\"file_path\":\"\",\"code\":\"SERIALIZATION_FAILED\",\"message\":\"Failed to serialize edit success payload\",\"edit_index\":null,\"current_version\":null,\"actual_text\":null}".into()
    })
}

fn serialize_failure(failure: &EditFailure) -> String {
    serde_json::to_string(failure).unwrap_or_else(|_| {
        "{\"ok\":false,\"file_path\":\"\",\"code\":\"SERIALIZATION_FAILED\",\"message\":\"Failed to serialize edit failure payload\",\"edit_index\":null,\"current_version\":null,\"actual_text\":null}".into()
    })
}

async fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes).await
}

fn resolve_existing_workspace_path(
    ctx: &ToolContext,
    input: &str,
) -> std::result::Result<PathBuf, ToolResult> {
    resolve_workspace_path(ctx, input, true)
}

fn resolve_workspace_path(
    ctx: &ToolContext,
    input: &str,
    require_exists: bool,
) -> std::result::Result<PathBuf, ToolResult> {
    let candidate = Path::new(input);
    if candidate.is_absolute() {
        return Err(ToolResult::error(
            "Absolute paths are not allowed; use a workspace-relative path.",
        ));
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolResult::error(
            "Path traversal with `..` is not allowed.",
        ));
    }

    let path = ctx.working_dir.join(candidate);
    if require_exists && !path.exists() {
        return Err(ToolResult::error(format!(
            "File not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

fn restore_previous_snapshot(
    session_id: &str,
    previous: Option<LastEditSnapshot>,
) -> Option<LastEditSnapshot> {
    match previous {
        Some(snapshot) => LAST_EDIT_SNAPSHOT_REGISTRY.insert(session_id.to_string(), snapshot),
        None => LAST_EDIT_SNAPSHOT_REGISTRY
            .remove(session_id)
            .map(|(_, snapshot)| snapshot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn test_ctx(working_dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: working_dir.to_path_buf(),
            session_id: format!("edit-test-{}", Uuid::new_v4()),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    fn write_text(tmp: &TempDir, rel_path: &str, content: &str) {
        std::fs::write(tmp.path().join(rel_path), content).unwrap();
    }

    fn write_bytes_fixture(tmp: &TempDir, rel_path: &str, content: &[u8]) {
        std::fs::write(tmp.path().join(rel_path), content).unwrap();
    }

    fn read_text(tmp: &TempDir, rel_path: &str) -> String {
        std::fs::read_to_string(tmp.path().join(rel_path)).unwrap()
    }

    fn read_bytes_fixture(tmp: &TempDir, rel_path: &str) -> Vec<u8> {
        std::fs::read(tmp.path().join(rel_path)).unwrap()
    }

    async fn run_edit_request(
        ctx: &ToolContext,
        file_path: &str,
        base_version: String,
        edits: Vec<Value>,
    ) -> (ToolResult, Value) {
        let tool = EditTool;
        let result = tool
            .execute(
                json!({
                    "file_path": file_path,
                    "base_version": base_version,
                    "edits": edits,
                }),
                ctx,
            )
            .await;

        let payload: Value = serde_json::from_str(&result.content).unwrap();
        (result, payload)
    }

    #[tokio::test]
    async fn exact_replace() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 12,
                "expected_text": "world",
                "new_text": "there"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["applied_edits"], json!(1));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello there\n");
        assert_eq!(
            payload["new_version"],
            json!(compute_version("hello there\n".as_bytes()))
        );
    }

    #[tokio::test]
    async fn insertion() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 7,
                "expected_text": "",
                "new_text": "big "
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello big world\n");
    }

    #[tokio::test]
    async fn deletion() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello big world\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello big world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 11,
                "expected_text": "big ",
                "new_text": ""
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn multi_edit_atomic_success() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "alpha\nbeta\ngamma\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("alpha\nbeta\ngamma\n".as_bytes()),
            vec![
                json!({
                    "start_line": 2,
                    "start_column": 1,
                    "end_line": 2,
                    "end_column": 5,
                    "expected_text": "beta",
                    "new_text": "BETA"
                }),
                json!({
                    "start_line": 1,
                    "start_column": 1,
                    "end_line": 1,
                    "end_column": 6,
                    "expected_text": "alpha",
                    "new_text": "ALPHA"
                }),
                json!({
                    "start_line": 3,
                    "start_column": 1,
                    "end_line": 3,
                    "end_column": 1,
                    "expected_text": "",
                    "new_text": ">> "
                }),
            ],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(payload["applied_edits"], json!(3));
        assert_eq!(read_text(&tmp, "sample.txt"), "ALPHA\nBETA\n>> gamma\n");
    }

    #[tokio::test]
    async fn overlap_rejection() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "abcdef\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("abcdef\n".as_bytes()),
            vec![
                json!({
                    "start_line": 1,
                    "start_column": 1,
                    "end_line": 1,
                    "end_column": 4,
                    "expected_text": "abc",
                    "new_text": "X"
                }),
                json!({
                    "start_line": 1,
                    "start_column": 2,
                    "end_line": 1,
                    "end_column": 5,
                    "expected_text": "bcd",
                    "new_text": "Y"
                }),
            ],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("OVERLAPPING_EDITS"));
        assert_eq!(read_text(&tmp, "sample.txt"), "abcdef\n");
    }

    #[tokio::test]
    async fn version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            "blake3:deadbeef".into(),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 12,
                "expected_text": "world",
                "new_text": "there"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("VERSION_MISMATCH"));
        assert_eq!(
            payload["current_version"],
            json!(compute_version("hello world\n".as_bytes()))
        );
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn expected_text_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 12,
                "expected_text": "earth",
                "new_text": "there"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("EXPECTED_TEXT_MISMATCH"));
        assert_eq!(payload["actual_text"], json!("world"));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn crlf_preservation() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes_fixture(&tmp, "sample.txt", b"one\r\ntwo\r\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(b"one\r\ntwo\r\n"),
            vec![json!({
                "start_line": 2,
                "start_column": 1,
                "end_line": 2,
                "end_column": 4,
                "expected_text": "two",
                "new_text": "dos"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_bytes_fixture(&tmp, "sample.txt"), b"one\r\ndos\r\n");
    }

    #[tokio::test]
    async fn unicode_correctness() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "a🙂z\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("a🙂z\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 2,
                "end_line": 1,
                "end_column": 3,
                "expected_text": "🙂",
                "new_text": "ß"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "aßz\n");
    }

    #[tokio::test]
    async fn eof_insertion() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "abc");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("abc".as_bytes()),
            vec![json!({
                "start_line": 2,
                "start_column": 1,
                "end_line": 2,
                "end_column": 1,
                "expected_text": "",
                "new_text": "\n"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "abc\n");
    }

    #[tokio::test]
    async fn empty_file_insert() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(b""),
            vec![json!({
                "start_line": 1,
                "start_column": 1,
                "end_line": 1,
                "end_column": 1,
                "expected_text": "",
                "new_text": "hello\n"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(payload["applied_edits"], json!(1));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello\n");
    }
}
