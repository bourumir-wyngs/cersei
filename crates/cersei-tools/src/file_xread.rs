//! Read tool: session-scoped tagged file reads backed by XFileStorage.

use super::*;
use crate::pdf_tool::is_pdf_path;
use crate::xfile_storage::{ensure_loaded, resolve_xfile_path, xfile_session_id, XFile, XLine};
use regex::Regex;
use serde::Deserialize;
use std::path::Path;

static READ_COUNTER_REGISTRY: once_cell::sync::Lazy<dashmap::DashMap<String, usize>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);
const DEFAULT_READ_LIMIT: usize = 100;

pub fn clear_read_counters(session_id: &str) {
    READ_COUNTER_REGISTRY.remove(session_id);
}

pub struct XReadTool;

/// Public alias preserved for downstream imports.
pub type FileXReadTool = XReadTool;

struct ReadSelection<'a> {
    lines: Vec<&'a XLine>,
    remaining_lines: usize,
    next_tag: Option<&'a str>,
}

struct SearchChunk<'a> {
    lines: Vec<&'a XLine>,
}

enum ReadOutputLine<'a> {
    Content(&'a XLine),
    Separator,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct XReadRequest {
    pub file_path: String,
    #[serde(default)]
    pub start_tag: Option<String>,
    #[serde(default)]
    pub end_tag: Option<String>,
    #[serde(default)]
    pub before: Option<usize>,
    #[serde(default)]
    pub after: Option<usize>,
    #[serde(default, alias = "length")]
    pub limit: Option<usize>,
    /// Optional Rust `regex` pattern. When set and non-empty, Read first selects
    /// lines using the normal range/windowing rules (ignoring `limit`), then
    /// filters the selection to matching lines plus `before`/`after` context.
    #[serde(default)]
    pub search: Option<String>,
    /// Internal flag to suppress the MultiRead nudge when called from MultiRead.
    #[serde(default)]
    pub suppress_nudge: Option<bool>,
}

#[async_trait]
impl Tool for XReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read the latest revision of a file. Open-ended reads return up to 100 lines by default. Use this to examine specific sections of code or retrieve line tags for editing. For reading multiple files, use MultiRead for better efficiency."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
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
                    "description": "Path to the target file. Absolute paths and workspace-relative paths are accepted."
                },
                "start_tag": {
                    "type": "string",
                    "description": "Optional starting tag from previous Read or Grep output. If omitted, Read starts from the beginning of the file."
                },
                "end_tag": {
                    "type": "string",
                    "description": "Optional inclusive ending tag from previous Read or Grep output. Use with `start_tag`."
                },
                "before": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional number of additional lines to include before `start_tag`. When `end_tag` is also provided, this expands the selected range backward from `start_tag`."
                },
                "after": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional number of additional lines to include after `end_tag`, or after `start_tag` when `end_tag` is omitted."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum number of lines to return. Defaults to 100 when `end_tag` is not set. Ignored when `end_tag` is set."
                },
                "search": {
                    "type": "string",
                    "description": "Optional Rust `regex` pattern. If set and non-empty, Read will first compute the normal selection (ignoring `limit`), then include only matching lines plus `before`/`after` context, separating match groups with `---------------`. Finally, `limit` is applied to the resulting output unless `end_tag` is set."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: XReadRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };
        let before = req.before.filter(|count| *count > 0);
        let after = req.after.filter(|count| *count > 0);
        let search = req
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let effective_limit = if req.end_tag.is_some() {
            None
        } else {
            Some(req.limit.unwrap_or(DEFAULT_READ_LIMIT))
        };

        let path = resolve_xfile_path(ctx, &req.file_path);
        if is_pdf_path(&path) {
            return ToolResult::error("Use PdfRead tool to read this format");
        }
        if is_spreadsheet_path(&path) {
            return ToolResult::error("Use SpreadSheet tool to read this format");
        }
        let storage_session_id = xfile_session_id(ctx);
        let head = match ensure_loaded(&storage_session_id, &path).await {
            Ok(head) => head,
            Err(err) => return ToolResult::error(err),
        };
        if !head.file.exists {
            return ToolResult::success(
                "File is absent in the current revision. Use Write to recreate it.",
            )
            .with_metadata(serde_json::json!({
                "file_path": head.file.path.display().to_string(),
                "current_version": head.current_version,
                "line_count": 0,
                "selected_count": 0,
                "exists": false,
            }));
        }

        let selected = match select_lines(
            &head.file,
            req.start_tag.as_deref(),
            req.end_tag.as_deref(),
            before,
            after,
            if search.is_some() {
                None
            } else {
                effective_limit
            },
        ) {
            Ok(lines) => lines,
            Err(err) => return ToolResult::error(err),
        };

        let (content, selected_count) = if let Some(pattern) = search {
            let regex = match Regex::new(pattern) {
                Ok(regex) => regex,
                Err(err) => return ToolResult::error(format!("Invalid regex: {}", err)),
            };

            let mut output_lines = render_search_lines(
                &selected.lines,
                &regex,
                before.unwrap_or(0),
                after.unwrap_or(0),
            );

            if output_lines.is_empty() {
                ("No matches found.".to_string(), 0)
            } else {
                let mut limit_note = None;
                if let Some(limit) = effective_limit {
                    if output_lines.len() > limit {
                        let truncated = output_lines.len() - limit;
                        output_lines.truncate(limit);
                        limit_note = Some(format!(
                            "Output truncated by {} line(s). Set `limit` field to return more lines if needed.",
                            truncated
                        ));
                    }
                }

                let selected_count = output_lines
                    .iter()
                    .filter(|line| matches!(line, ReadOutputLine::Content(_)))
                    .count();

                let mut content = render_output_lines(&output_lines);
                if let Some(note) = limit_note {
                    content.push_str("\n\n");
                    content.push_str(&note);
                }
                (content, selected_count)
            }
        } else {
            let mut content = render_xlines(&selected.lines);
            if let Some(next_tag) = selected.next_tag {
                if !content.is_empty() {
                    content.push_str("\n\n");
                }
                content.push_str(&format!(
                    "File truncated, {} lines remaining, next line tag {}. Set `limit` field to return more lines if needed.",
                    selected.remaining_lines, next_tag
                ));
            }
            (content, selected.lines.len())
        };

        let mut final_content = content;
        if !req.suppress_nudge.unwrap_or(false) {
            let read_count = {
                let mut entry = READ_COUNTER_REGISTRY
                    .entry(ctx.session_id.clone())
                    .or_insert(0);
                *entry += 1;
                *entry
            };

            if read_count >= 3 && read_count % 3 == 0 {
                final_content.push_str("\n\nNOTE: You have called Read multiple times in this session. For better efficiency, consider using MultiRead to gather context from multiple files in a single request.");
            }
        }

        ToolResult::success(final_content).with_metadata(serde_json::json!({
            "file_path": head.file.path.display().to_string(),
            "current_version": head.current_version,
            "line_count": head.file.content.len(),
            "selected_count": selected_count,
            "exists": true,
        }))
    }
}

fn render_xlines(lines: &[&XLine]) -> String {
    lines
        .iter()
        .map(|line| format!("{:>6}\t{}", line.tag, line.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_search_lines<'a>(
    selected_lines: &[&'a XLine],
    regex: &Regex,
    before: usize,
    after: usize,
) -> Vec<ReadOutputLine<'a>> {
    let chunks = search_selected_lines(selected_lines, regex, before, after);
    let mut output_lines = Vec::new();

    for (idx, chunk) in chunks.iter().enumerate() {
        if idx > 0 {
            output_lines.push(ReadOutputLine::Separator);
        }
        output_lines.extend(chunk.lines.iter().copied().map(ReadOutputLine::Content));
    }

    output_lines
}

fn render_output_lines(lines: &[ReadOutputLine<'_>]) -> String {
    lines
        .iter()
        .map(|line| match line {
            ReadOutputLine::Content(line) => format!("{:>6}\t{}", line.tag, line.content),
            ReadOutputLine::Separator => "---------------".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn select_lines<'a>(
    file: &'a XFile,
    start_tag: Option<&str>,
    end_tag: Option<&str>,
    before: Option<usize>,
    after: Option<usize>,
    limit: Option<usize>,
) -> std::result::Result<ReadSelection<'a>, String> {
    if file.content.is_empty() {
        if let Some(start_tag) = start_tag {
            return Err(format!(
                "Unknown start_tag '{}' in {}. Tags are globally unique, this file starts from <empty file>",
                start_tag,
                file.path.display()
            ));
        }
        if let Some(end_tag) = end_tag {
            return Err(format!(
                "Unknown end_tag '{}' in {}",
                end_tag,
                file.path.display()
            ));
        }

        return Ok(ReadSelection {
            lines: Vec::new(),
            remaining_lines: 0,
            next_tag: None,
        });
    }

    let start_idx = if let Some(start_tag) = start_tag {
        file.content
            .iter()
            .position(|line| line.tag == start_tag)
            .ok_or_else(|| {
                let first_tag = file
                    .content
                    .first()
                    .map(|line| line.tag.as_str())
                    .unwrap_or("<empty file>");
                format!(
                    "Unknown start_tag '{}' in {}. Tags are globally unique, this file starts from {}",
                    start_tag,
                    file.path.display(),
                    first_tag
                )
            })?
    } else {
        0
    };

    let last_idx = file.content.len() - 1;
    let raw_end_idx = if let Some(end_tag) = end_tag {
        let end_idx = file
            .content
            .iter()
            .position(|line| line.tag == end_tag)
            .ok_or_else(|| format!("Unknown end_tag '{}' in {}", end_tag, file.path.display()))?;
        if end_idx < start_idx {
            return Err("Read end_tag must not come before start_tag.".to_string());
        }
        Some(end_idx)
    } else {
        None
    };

    let range_start = if start_tag.is_some() {
        start_idx.saturating_sub(before.unwrap_or(0))
    } else {
        0
    };
    let range_end = if let Some(end_idx) = raw_end_idx {
        end_idx.saturating_add(after.unwrap_or(0)).min(last_idx)
    } else if start_tag.is_some() && (before.is_some() || after.is_some()) {
        start_idx.saturating_add(after.unwrap_or(0)).min(last_idx)
    } else {
        last_idx
    };

    let range_len = range_end - range_start + 1;
    let actual_len = if let Some(limit) = limit {
        range_len.min(limit)
    } else {
        range_len
    };

    let next_idx = range_start + actual_len;
    let remaining_lines = range_len - actual_len;

    Ok(ReadSelection {
        lines: file.content[range_start..next_idx].iter().collect(),
        remaining_lines,
        next_tag: if remaining_lines > 0 {
            Some(file.content[next_idx].tag.as_str())
        } else {
            None
        },
    })
}

fn search_selected_lines<'a>(
    selected_lines: &[&'a XLine],
    regex: &Regex,
    before: usize,
    after: usize,
) -> Vec<SearchChunk<'a>> {
    let match_indices: Vec<usize> = selected_lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| regex.is_match(&line.content).then_some(idx))
        .collect();

    if match_indices.is_empty() {
        return Vec::new();
    }

    let last_idx = selected_lines.len().saturating_sub(1);
    let mut chunks = Vec::new();

    for &match_idx in &match_indices {
        let start = match_idx.saturating_sub(before);
        let end = match_idx.saturating_add(after).min(last_idx);
        chunks.push(SearchChunk {
            lines: selected_lines[start..=end].to_vec(),
        });
    }

    chunks
}

fn is_spreadsheet_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "xls" | "xlsx" | "ods"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{
        clear_session_xfile_storage, store_deleted_file, store_written_text,
    };
    use std::path::Path;
    use std::sync::Arc;
    use uuid::Uuid;

    fn test_ctx(root: &Path, session_id: &str) -> ToolContext {
        ToolContext {
            working_dir: root.to_path_buf(),
            session_id: session_id.into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    #[test]
    fn xread_schema_exposes_tag_range_inputs() {
        let tool = XReadTool;
        let schema = tool.input_schema();

        assert_eq!(schema["properties"]["search"]["type"], "string");
        assert_eq!(schema["properties"]["start_tag"]["type"], "string");
        assert_eq!(schema["properties"]["end_tag"]["type"], "string");
        assert_eq!(schema["properties"]["before"]["minimum"], 0);
        assert_eq!(schema["properties"]["after"]["minimum"], 0);
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
    }

    #[test]
    fn filesystem_toolset_includes_xread() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "Read"));
    }
    #[tokio::test]
    async fn xread_rejects_spreadsheet_files() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-spreadsheet-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("sheet.xlsx");
        tokio::fs::write(&path, b"placeholder").await.unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, "Use SpreadSheet tool to read this format");
    }

    #[tokio::test]
    async fn xread_rejects_pdf_files() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-pdf-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join("pdf.pdf");
        let path = tmp.path().join("doc.pdf");
        tokio::fs::copy(&source, &path).await.unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, "Use PdfRead tool to read this format");
    }

    #[tokio::test]
    async fn xread_search_filters_lines_without_requiring_start_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let head = store_written_text(&session_id, &path, "zero\nfoo one\nctx a\nfoo two\nend\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "search": "foo",
                    "before": 1,
                    "after": 1
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tzero"));
        assert!(result.content.contains("\tfoo one"));
        assert!(result.content.contains("\tctx a"));
        assert!(result.content.contains("---------------"));
        assert!(result.content.contains("\tfoo two"));
        assert!(result.content.contains("\tend"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 6);
    }

    #[tokio::test]
    async fn xread_reports_absent_current_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-absent-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("missing.txt");
        let head = store_written_text(&session_id, &path, "hello\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();
        store_deleted_file(&session_id, &path);
        tokio::fs::remove_file(&path).await.unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result
            .content
            .contains("File is absent in the current revision"));
        assert_eq!(result.metadata.as_ref().unwrap()["exists"], false);
    }

    #[tokio::test]
    async fn xread_search_applies_limit_to_final_output() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-limit-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let head = store_written_text(&session_id, &path, "zero\nfoo one\nctx a\nfoo two\nend\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "search": "foo",
                    "before": 1,
                    "after": 1,
                    "limit": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tzero"));
        assert!(result.content.contains("\tfoo one"));
        assert!(result
            .content
            .contains("Set `limit` field to return more lines if needed."));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_search_ignores_limit_when_end_tag_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-range-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let head = store_written_text(&session_id, &path, "zero\nfoo\nskip\nbar\nend\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[0].tag,
                    "end_tag": head.file.content[3].tag,
                    "search": "foo|bar",
                    "limit": 1
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tfoo"));
        assert!(result.content.contains("---------------"));
        assert!(result.content.contains("\tbar"));
        assert_eq!(result.content.lines().count(), 3);
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_search_reports_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-empty-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let head = store_written_text(&session_id, &path, "zero\none\ntwo\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "search": "foo"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "No matches found.");
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 0);
    }

    #[tokio::test]
    async fn xread_search_rejects_invalid_regex() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-invalid-regex-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let head = store_written_text(&session_id, &path, "zero\none\ntwo\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "search": "("
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.starts_with("Invalid regex:"));
    }

    #[tokio::test]
    async fn xread_search_allows_end_tag_without_start_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-end-tag-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let head = store_written_text(&session_id, &path, "foo\nskip\nbar\nend\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "end_tag": head.file.content[2].tag,
                    "search": "foo|bar"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tfoo"));
        assert!(result.content.contains("---------------"));
        assert!(result.content.contains("\tbar"));
        assert!(!result.content.contains("\tend"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_search_respects_selected_tag_range() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-range-bounds-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let head = store_written_text(
            &session_id,
            &path,
            "foo outside before\ninside one\ninside two\nfoo outside after\n",
        );
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[1].tag,
                    "end_tag": head.file.content[2].tag,
                    "search": "foo"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "No matches found.");
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 0);
    }

    #[tokio::test]
    async fn xread_returns_tagged_content() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "alpha\nbeta\ngamma\n")
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\talpha"));
        assert!(result.content.contains("\tbeta"));
        assert!(result.metadata.as_ref().unwrap()["current_version"].is_string());
    }

    #[tokio::test]
    async fn xread_supports_inclusive_tag_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("range.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[0].tag,
                    "end_tag": head.file.content[1].tag
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(!result.content.contains("\tthree"));
    }

    #[tokio::test]
    async fn xread_end_tag_without_start_tag_reads_from_beginning_of_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("range.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "end_tag": head.file.content[1].tag
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(!result.content.contains("\tthree"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_rejects_unknown_start_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("unknown-start.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": "tag:missing"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            format!(
                "Unknown start_tag 'tag:missing' in {}. Tags are globally unique, this file starts from {}",
                path.display(),
                head.file.content[0].tag
            )
        );
    }

    #[tokio::test]
    async fn xread_limit_without_start_tag_reads_from_beginning_of_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "limit": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(!result.content.contains("\tthree"));
        assert!(result.content.contains(&format!(
            "File truncated, 2 lines remaining, next line tag {}. Set `limit` field to return more lines if needed.",
            head.file.content[2].tag
        )));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_rejects_unknown_end_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("unknown-end.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[0].tag,
                    "end_tag": "tag:missing"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            format!("Unknown end_tag 'tag:missing' in {}", path.display())
        );
    }

    #[tokio::test]
    async fn xread_ignores_limit_when_end_tag_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("range.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[0].tag,
                    "end_tag": head.file.content[2].tag,
                    "limit": 1
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(!result.content.contains("File truncated,"));
    }

    #[tokio::test]
    async fn xread_start_tag_without_end_reads_to_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("tail.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[1].tag
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(!result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_with_before_window() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\nfive\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[3].tag,
                    "before": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(result.content.contains("\tfour"));
        assert!(!result.content.contains("\tone"));
        assert!(!result.content.contains("\tfive"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 3);
    }

    #[tokio::test]
    async fn xread_with_after_window() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\nfive\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[1].tag,
                    "after": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(!result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(result.content.contains("\tfour"));
        assert!(!result.content.contains("\tfive"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 3);
    }

    #[tokio::test]
    async fn xread_with_before_and_after_window() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(
            &session_id,
            &path,
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
        );
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[3].tag,
                    "end_tag": head.file.content[4].tag,
                    "before": 1,
                    "after": 1
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tthree"));
        assert!(result.content.contains("\tfour"));
        assert!(result.content.contains("\tfive"));
        assert!(result.content.contains("\tsix"));
        assert!(!result.content.contains("\ttwo"));
        assert!(!result.content.contains("\tone"));
        assert!(!result.content.contains("\tseven"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 4);
    }

    #[tokio::test]
    async fn xread_before_clips_to_file_start() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[1].tag,
                    "before": 10
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(!result.content.contains("\tthree"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_after_clips_to_file_end() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[1].tag,
                    "after": 10
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(!result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_with_explicit_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\nfive\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[0].tag,
                    "before": 1,
                    "after": 2,
                    "limit": 3
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(!result.content.contains("\tfour"));
        assert!(!result.content.contains("File truncated,"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 3);
    }

    #[tokio::test]
    async fn xread_reports_next_tag_when_limit_truncates_output() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\nfive\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[1].tag,
                    "limit": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(!result.content.contains("\tfour"));
        assert!(result.content.contains(&format!(
            "File truncated, 2 lines remaining, next line tag {}. Set `limit` field to return more lines if needed.",
            head.file.content[3].tag
        )));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_defaults_to_100_lines_for_open_ended_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-default-limit-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("long.txt");
        let mut lines = String::new();
        for idx in 0..105 {
            lines.push_str(&format!("line {idx}\n"));
        }
        let head = store_written_text(&session_id, &path, &lines);
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tline 0"));
        assert!(result.content.contains("\tline 99"));
        assert!(!result.content.contains("\tline 100"));
        assert!(result.content.contains(&format!(
            "File truncated, 5 lines remaining, next line tag {}. Set `limit` field to return more lines if needed.",
            head.file.content[100].tag
        )));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 100);
    }

    #[tokio::test]
    async fn xread_search_defaults_to_100_lines_when_not_bounded_by_end_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-search-default-limit-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("search.txt");
        let mut lines = String::new();
        for idx in 0..105 {
            lines.push_str(&format!("match {idx}\n"));
        }
        let head = store_written_text(&session_id, &path, &lines);
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "search": "match"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result
            .content
            .contains(&format!("\t{}", head.file.content[0].content)));
        assert!(result
            .content
            .contains(&format!("\t{}", head.file.content[49].content)));
        assert!(!result
            .content
            .contains(&format!("\t{}", head.file.content[50].content)));
        assert!(result
            .content
            .contains("Set `limit` field to return more lines if needed."));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 50);
    }

    #[tokio::test]
    async fn xread_windowing_without_start_tag_reads_from_beginning_of_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "after": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(result.content.contains("\tfour"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 4);
    }

    #[tokio::test]
    async fn xread_zero_window_values_preserve_start_to_eof_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("window.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[1].tag,
                    "before": 0,
                    "after": 0
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(!result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_rejects_reversed_tag_range() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("reverse.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[2].tag,
                    "end_tag": head.file.content[1].tag,
                    "before": 10
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            "Read end_tag must not come before start_tag."
        );
    }
}
