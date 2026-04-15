//! Exact-position file editing plus XFileStorage-backed revert.

use super::*;
use crate::file_history::{unified_diff, FileHistory};
use crate::xfile_storage::{
    discard_head_revision, list_revisions, record_disk_state, render_file, resolve_xfile_path,
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;

const EXPECTED_TEXT_SEARCH_WINDOW_BYTES: usize = 256;

#[derive(Debug, Clone)]
struct LastEditSnapshot {
    file_path: PathBuf,
}

static LAST_EDIT_SNAPSHOT_REGISTRY: Lazy<dashmap::DashMap<String, LastEditSnapshot>> =
    Lazy::new(dashmap::DashMap::new);

pub struct EditTool;
pub struct RevertTool;

/// Public alias preserved for downstream imports.
pub type FileEditTool = EditTool;

#[derive(Debug, Clone, Deserialize)]
pub struct EditRequest {
    pub file_path: String,
    #[serde(default)]
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
         expected_text for each edit. Read returns version metadata to bootstrap the first exact edit. A missing base_version is allowed only on the first Edit call for a file in a session; later calls must provide version metadata. Returns structured JSON."
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
                    "description": "Exact file version hash in the form `blake3:<hex>`. Read returns this in metadata for Edit. You may omit it only on the first Edit call for a file in a session; after that, provide version metadata. Requests fail if the file has changed."
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
            "required": ["file_path", "edits"]
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
impl Tool for RevertTool {
    fn name(&self) -> &str {
        "Revert"
    }

    fn description(&self) -> &str {
        "Restore the immediately previous XFileStorage revision for a tracked file in this session. `file_path` is required. Revert works only for files already loaded into XFileStorage by Read, Write, Edit, or matching Grep results. On success, Revert flushes the restored content to disk, removes the current head revision from XFileStorage, and returns a unified diff from the old head to the restored revision."
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
                    "description": "Required path to the tracked file to revert. Absolute paths and workspace-relative paths are accepted."
                }
            },
            "required": ["file_path"]
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

        let requested = match input.file_path.as_ref() {
            Some(requested) => requested,
            None => {
                return ToolResult::error("file_path is required for XFileStorage-backed revert.");
            }
        };
        let requested_path = resolve_xfile_path(ctx, requested);
        let revisions = match list_revisions(&ctx.session_id, &requested_path) {
            Some(revisions) if revisions.len() >= 2 => revisions,
            Some(_) => {
                return ToolResult::error(format!(
                    "No previous XFileStorage revision is available to revert for {}.",
                    requested_path.display()
                ));
            }
            None => {
                return ToolResult::error(format!(
                    "File is not loaded in XFileStorage: {}",
                    requested_path.display()
                ));
            }
        };
        let current = revisions
            .last()
            .expect("checked revision list is non-empty");
        let previous = &revisions[revisions.len() - 2];
        let current_text = render_file(&current.file);
        let restored_text = render_file(&previous.file);
        if let Err(e) = tokio::fs::write(&requested_path, restored_text.as_bytes()).await {
            return ToolResult::error(format!("Failed to restore file: {}", e));
        }
        if let Err(err) = record_disk_state(&ctx.session_id, &requested_path) {
            return ToolResult::error(err);
        }
        if let Err(err) = discard_head_revision(&ctx.session_id, &requested_path) {
            return ToolResult::error(err);
        }
        let diff = unified_diff(
            &current_text,
            &restored_text,
            &format!("{} (current)", requested_path.display()),
            &format!("{} (reverted)", requested_path.display()),
        );

        ToolResult::success(format!(
            "Reverted {} to the previous XFileStorage revision.\n{}",
            requested_path.display(),
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
    let (_bytes, text, version) = read_file(path, &req.file_path).await?;

    let missing_base_version = req.base_version.trim().is_empty();
    if missing_base_version && !is_first_edit_for_session_file(ctx, path) {
        return Err(failure(
            &req.file_path,
            "VERSION_REQUIRED",
            format!(
                "Provide version metadata from Read before editing {} again.",
                req.file_path
            ),
            None,
            Some(version.clone()),
            None,
        ));
    }

    if !missing_base_version && version != req.base_version {
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

fn is_first_edit_for_session_file(ctx: Option<&ToolContext>, path: &Path) -> bool {
    let Some(ctx) = ctx else {
        return false;
    };

    match LAST_EDIT_SNAPSHOT_REGISTRY.get(&ctx.session_id) {
        Some(snapshot) => snapshot.file_path != path,
        None => true,
    }
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
        return Err("String indices are 1 based.".into());
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

fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn ceil_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn unique_nearby_expected_match(
    text: &str,
    start: usize,
    end: usize,
    expected_text: &str,
) -> Option<(usize, usize)> {
    if expected_text.is_empty() {
        return None;
    }

    let search_start = floor_char_boundary(
        text,
        start.saturating_sub(EXPECTED_TEXT_SEARCH_WINDOW_BYTES),
    );
    let search_end = ceil_char_boundary(
        text,
        end.saturating_add(expected_text.len())
            .saturating_add(EXPECTED_TEXT_SEARCH_WINDOW_BYTES),
    );
    let search = &text[search_start..search_end];

    let mut matches = search.match_indices(expected_text);
    let (first_match, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }

    let matched_start = search_start + first_match;
    Some((matched_start, matched_start + expected_text.len()))
}

fn mismatch_context(text: &str, start: usize, end: usize) -> String {
    let context_start = floor_char_boundary(text, start.saturating_sub(40));
    let context_end = ceil_char_boundary(text, end.saturating_add(40));
    text[context_start..context_end].escape_debug().to_string()
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
                    format!(
                        "Invalid start position for edit {}: {}",
                        edit_index, message
                    ),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    None,
                )
            },
        )?;

        let end =
            position_to_offset(text, index, edit.end_line, edit.end_column).map_err(|message| {
                failure(
                    file_path,
                    "INVALID_POSITION",
                    format!("Invalid end position for edit {}: {}", edit_index, message),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    None,
                )
            })?;

        if start > end {
            return Err(failure(
                file_path,
                "INVALID_RANGE",
                format!(
                    "Edit {} start position is after its end position.",
                    edit_index
                ),
                Some(edit_index),
                Some(current_version.to_string()),
                None,
            ));
        }

        let actual = &text[start..end];
        let (start, end) = if actual != edit.expected_text {
            if let Some((matched_start, matched_end)) =
                unique_nearby_expected_match(text, start, end, &edit.expected_text)
            {
                (matched_start, matched_end)
            } else {
                return Err(failure(
                    file_path,
                    "EXPECTED_TEXT_MISMATCH",
                    format!(
                        "Edit {} expected text does not match the current file content at byte range {}..{}. Nearby context: \"{}\".",
                        edit_index,
                        start,
                        end,
                        mismatch_context(text, start, end)
                    ),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    Some(actual.to_string()),
                ));
            }
        } else {
            (start, end)
        };

        if &text[start..end] != edit.expected_text {
            return Err(failure(
                file_path,
                "EXPECTED_TEXT_MISMATCH",
                format!(
                    "Edit {} expected text does not match the current file content at byte range {}..{}. Nearby context: \"{}\".",
                    edit_index,
                    start,
                    end,
                    mismatch_context(text, start, end)
                ),
                Some(edit_index),
                Some(current_version.to_string()),
                Some(text[start..end].to_string()),
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
    tmp.persist(path).map_err(|err| {
        write_failure(
            file_path,
            format!("Cannot persist temp file: {}", err.error),
        )
    })?;

    Ok(())
}

fn make_diff(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new).unified_diff().to_string()
}

fn write_failure(file_path: &str, message: String) -> EditFailure {
    failure(file_path, "WRITE_FAILED", message, None, None, None)
}

fn resolve_edit_path(ctx: &ToolContext, input: &str) -> std::result::Result<PathBuf, EditFailure> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_xedit::XEditTool;
    use crate::file_xwrite::XWriteTool;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::try_get_head;
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
        if let Some(parent) = tmp.path().join(rel_path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(tmp.path().join(rel_path), content).unwrap();
    }

    fn write_bytes_fixture(tmp: &TempDir, rel_path: &str, content: &[u8]) {
        if let Some(parent) = tmp.path().join(rel_path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
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
    async fn zero_indices_return_one_based_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 0,
                "start_column": 1,
                "end_line": 1,
                "end_column": 1,
                "expected_text": "",
                "new_text": "X"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("INVALID_POSITION"));
        assert_eq!(
            payload["message"],
            json!("Invalid start position for edit 0: String indices are 1 based.")
        );
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
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

    #[tokio::test]
    async fn nearby_attribute_boundary_match_recovers_after_prior_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
pub struct EditTool;\n\
pub struct LegacyTool;\n\
\n\
#[async_trait]\n\
impl Tool for LegacyTool {\n\
    fn name(&self) -> &str {\n\
        \"Legacy\"\n\
    }\n\
}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (first_result, first_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 3,
                "start_column": 1,
                "end_line": 3,
                "end_column": 1,
                "expected_text": "",
                "new_text": "pub struct HelperTool;\n"
            })],
        )
        .await;

        assert!(!first_result.is_error, "{}", first_result.content);

        let after_first = read_text(&tmp, "sample.txt");
        let (second_result, second_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            first_payload["new_version"].as_str().unwrap().to_string(),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 4,
                "end_column": 1,
                "expected_text": "#[async_trait]\nimpl Tool for LegacyTool {",
                "new_text": "#[derive(Deserialize)]\nstruct HelperInput {\n    value: String,\n}\n\n#[async_trait]\nimpl Tool for LegacyTool {"
            })],
        )
        .await;

        assert!(!second_result.is_error, "{}", second_result.content);
        let updated = read_text(&tmp, "sample.txt");
        assert!(after_first.contains("#[async_trait]\nimpl Tool for LegacyTool {"));
        assert!(updated.contains("pub struct HelperTool;"));
        assert!(updated.contains("#[derive(Deserialize)]\nstruct HelperInput {\n    value: String,\n}\n\n#[async_trait]\nimpl Tool for LegacyTool {"));
        assert_eq!(second_payload["applied_edits"], json!(1));
    }

    #[tokio::test]
    async fn nearby_function_boundary_match_recovers_from_newline_only_slice() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
fn create_helper_dir() -> std::result::Result<(), String> {\n\
    Ok(())\n\
}\n\
\n\
fn helper_command() {}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 5,
                "end_column": 1,
                "expected_text": "fn helper_command(",
                "new_text": "fn inserted_helper() {}\n\nfn helper_command("
            })],
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(payload["applied_edits"], json!(1));
        assert_eq!(
            read_text(&tmp, "sample.txt"),
            "\
fn create_helper_dir() -> std::result::Result<(), String> {\n\
    Ok(())\n\
}\n\
\n\
fn inserted_helper() {}\n\
\n\
fn helper_command() {}\n"
        );
    }

    #[tokio::test]
    async fn nearby_match_remains_rejected_when_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
#[async_trait]\n\
impl Tool for LegacyTool {\n\
}\n\
\n\
#[async_trait]\n\
impl Tool for LegacyTool {\n\
}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 4,
                "end_column": 1,
                "expected_text": "#[async_trait]\nimpl Tool for LegacyTool {",
                "new_text": "ignored"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("EXPECTED_TEXT_MISMATCH"));
        assert_eq!(payload["actual_text"], json!(""));
        assert_eq!(read_text(&tmp, "sample.txt"), initial);
    }

    #[tokio::test]
    async fn sequential_boundary_edits_keep_versions_and_apply_nearby_match() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
alpha\n\
\n\
#[async_trait]\n\
impl Tool for LegacyTool {\n\
    fn name(&self) -> &str {\n\
        \"Legacy\"\n\
    }\n\
}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (first_result, first_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 2,
                "start_column": 1,
                "end_line": 2,
                "end_column": 1,
                "expected_text": "",
                "new_text": "pub struct HelperTool;\n"
            })],
        )
        .await;

        assert!(!first_result.is_error, "{}", first_result.content);
        let after_first = read_text(&tmp, "sample.txt");
        let first_version = first_payload["new_version"].as_str().unwrap().to_string();
        assert_eq!(first_version, compute_version(after_first.as_bytes()));

        let (second_result, second_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            first_version.clone(),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 4,
                "end_column": 1,
                "expected_text": "#[async_trait]\nimpl Tool for LegacyTool {",
                "new_text": "#[derive(Deserialize)]\nstruct HelperInput {\n    value: String,\n}\n\n#[async_trait]\nimpl Tool for LegacyTool {"
            })],
        )
        .await;

        assert!(!second_result.is_error, "{}", second_result.content);
        assert_eq!(second_payload["applied_edits"], json!(1));
        let final_text = read_text(&tmp, "sample.txt");
        assert!(final_text.contains("pub struct HelperTool;"));
        assert!(final_text.contains("#[derive(Deserialize)]\nstruct HelperInput {\n    value: String,\n}\n\n#[async_trait]\nimpl Tool for LegacyTool {"));
        assert_eq!(
            second_payload["new_version"],
            json!(compute_version(final_text.as_bytes()))
        );
    }

    #[tokio::test]
    async fn mismatch_reports_context_for_newline_boundary_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
fn create_helper_dir() -> std::result::Result<(), String> {\n\
    Ok(())\n\
}\n\
\n\
fn helper_command() {}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 5,
                "end_column": 1,
                "expected_text": "fn missing_command(",
                "new_text": "ignored"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("EXPECTED_TEXT_MISMATCH"));
        assert_eq!(payload["actual_text"], json!("\n"));
        let message = payload["message"].as_str().unwrap();
        assert!(message.contains("Nearby context:"));
        assert!(message.contains("fn helper_command() {}"));
        assert_eq!(read_text(&tmp, "sample.txt"), initial);
    }

    #[tokio::test]
    async fn revert_restores_previous_xwrite_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        let writer = XWriteTool;

        let first = writer
            .execute(
                json!({
                    "file_path": "sample.txt",
                    "content": "hello world\n",
                }),
                &ctx,
            )
            .await;
        assert!(!first.is_error, "{}", first.content);

        let second = writer
            .execute(
                json!({
                    "file_path": "sample.txt",
                    "content": "hello there\n",
                }),
                &ctx,
            )
            .await;
        assert!(!second.is_error, "{}", second.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello there\n");

        let revert_tool = RevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": "sample.txt" }), &ctx)
            .await;

        assert!(!revert.is_error, "{}", revert.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn revert_restores_previous_xedit_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        let writer = XWriteTool;
        let editor = XEditTool;

        let write = writer
            .execute(
                json!({
                    "file_path": "sample.txt",
                    "content": "alpha\nbeta\n",
                }),
                &ctx,
            )
            .await;
        assert!(!write.is_error, "{}", write.content);

        let path = tmp.path().join("sample.txt");
        let head = try_get_head(&ctx.session_id, &path).unwrap();
        let edit = editor
            .execute(
                json!({
                    "file_path": "sample.txt",
                    "base_version": head.current_version,
                    "operations": [{
                        "op": "replace_line",
                        "tag": head.file.content[1].tag,
                        "new_text": "BETA",
                    }],
                }),
                &ctx,
            )
            .await;
        assert!(!edit.is_error, "{}", edit.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "alpha\nBETA\n");

        let revert_tool = RevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": "sample.txt" }), &ctx)
            .await;

        assert!(!revert.is_error, "{}", revert.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn revert_rejects_untracked_non_xstorage_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let revert_tool = RevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": "sample.txt" }), &ctx)
            .await;

        assert!(revert.is_error);
        assert!(revert
            .content
            .contains("File is not loaded in XFileStorage"));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }
}
