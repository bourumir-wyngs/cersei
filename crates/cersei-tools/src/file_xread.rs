//! Read tool: session-scoped tagged file reads backed by XFileStorage.

use super::*;
use crate::xfile_storage::{ensure_loaded, resolve_xfile_path, XFile, XLine};
use serde::Deserialize;

pub struct XReadTool;

/// Public alias preserved for downstream imports.
pub type FileXReadTool = XReadTool;

struct ReadSelection<'a> {
    lines: Vec<&'a XLine>,
    remaining_lines: usize,
    next_tag: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default)]
    pub length: Option<usize>,
}

#[async_trait]
impl Tool for XReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read the latest revision of a file."
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
                    "description": "Optional starting tag from previous Read or Grep output. If no other parameters are given, Read returns text from this tag through end of file."
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
                "length": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional absolute limit on the number of lines to return. The read starts at the calculated position (based on start_tag/before and end_tag/after) and stops when this many lines have been returned."
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

        // Validate parameters
        if req.end_tag.is_some() && req.length.is_some() {
            return ToolResult::error("Read accepts either `end_tag` or `length`, not both.");
        }
        if req.start_tag.is_none()
            && (req.end_tag.is_some()
                || req.length.is_some()
                || before.is_some()
                || after.is_some())
        {
            return ToolResult::error(
                "Read requires `start_tag` when `end_tag`, `length`, `before`, or `after` is provided.",
            );
        }

        let path = resolve_xfile_path(ctx, &req.file_path);
        let head = match ensure_loaded(&ctx.session_id, &path).await {
            Ok(head) => head,
            Err(err) => return ToolResult::error(err),
        };

        let selected = match select_lines(
            &head.file,
            req.start_tag.as_deref(),
            req.end_tag.as_deref(),
            before,
            after,
            req.length,
        ) {
            Ok(lines) => lines,
            Err(err) => return ToolResult::error(err),
        };

        let mut content = selected
            .lines
            .iter()
            .map(|line| format!("{:>6}\t{}", line.tag, line.content))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(next_tag) = selected.next_tag {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&format!(
                "File truncated, {} lines remaining, next line tag {}",
                selected.remaining_lines, next_tag
            ));
        }

        ToolResult::success(content).with_metadata(serde_json::json!({
            "file_path": head.file.path.display().to_string(),
            "current_version": head.current_version,
            "line_count": head.file.content.len(),
            "selected_count": selected.lines.len(),
        }))
    }
}

fn select_lines<'a>(
    file: &'a XFile,
    start_tag: Option<&str>,
    end_tag: Option<&str>,
    before: Option<usize>,
    after: Option<usize>,
    length: Option<usize>,
) -> std::result::Result<ReadSelection<'a>, String> {
    if let Some(start_tag) = start_tag {
        let start_idx = file
            .content
            .iter()
            .position(|line| line.tag == start_tag)
            .ok_or_else(|| {
                let first_tag = file.content.first().map(|line| line.tag.as_str()).unwrap_or("<empty file>");
                format!(
                    "Unknown start_tag '{}' in {}. Tags are globally unique, this file starts from {}",
                    start_tag,
                    file.path.display(),
                    first_tag
                )
            })?;

        let last_idx = file.content.len() - 1;
        let raw_end_idx = if let Some(end_tag) = end_tag {
            let end_idx = file
                .content
                .iter()
                .position(|line| line.tag == end_tag)
                .ok_or_else(|| {
                    format!("Unknown end_tag '{}' in {}", end_tag, file.path.display())
                })?;
            if end_idx < start_idx {
                return Err("Read end_tag must not come before start_tag.".to_string());
            }
            Some(end_idx)
        } else {
            None
        };

        let range_start = start_idx.saturating_sub(before.unwrap_or(0));
        let range_end = if let Some(end_idx) = raw_end_idx {
            end_idx.saturating_add(after.unwrap_or(0)).min(last_idx)
        } else if before.is_some() || after.is_some() {
            start_idx.saturating_add(after.unwrap_or(0)).min(last_idx)
        } else {
            last_idx
        };

        let range_len = range_end - range_start + 1;
        let actual_len = if let Some(length) = length {
            range_len.min(length)
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
    } else {
        Ok(ReadSelection {
            lines: file.content.iter().collect(),
            remaining_lines: 0,
            next_tag: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{clear_session_xfile_storage, store_written_text};
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

        assert_eq!(schema["properties"]["start_tag"]["type"], "string");
        assert_eq!(schema["properties"]["end_tag"]["type"], "string");
        assert_eq!(schema["properties"]["before"]["minimum"], 0);
        assert_eq!(schema["properties"]["after"]["minimum"], 0);
        assert_eq!(schema["properties"]["length"]["minimum"], 1);
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
    async fn xread_requires_start_tag_when_length_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": "missing.txt",
                    "length": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            "Read requires `start_tag` when `end_tag`, `length`, `before`, or `after` is provided."
        );
    }

    #[tokio::test]
    async fn xread_rejects_end_tag_and_length_together() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("range.txt");
        let head = store_written_text(&session_id, &path, "one\ntwo\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "start_tag": head.file.content[0].tag,
                    "end_tag": head.file.content[1].tag,
                    "length": 1
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            "Read accepts either `end_tag` or `length`, not both."
        );
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
                    "start_tag": head.file.content[3].tag,  // four
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
                    "start_tag": head.file.content[1].tag,  // two
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
                    "start_tag": head.file.content[3].tag,  // four
                    "end_tag": head.file.content[4].tag,    // five
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
                    "start_tag": head.file.content[1].tag,  // two
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
                    "start_tag": head.file.content[1].tag,  // two
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
    async fn xread_with_length_limit() {
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
                    "length": 3
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        // Without length, the bounded window would contain one, two, and three.
        // With length=3, we still get the full calculated window.
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(!result.content.contains("\tfour"));
        assert!(!result.content.contains("File truncated,"));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 3);
    }

    #[tokio::test]
    async fn xread_reports_next_tag_when_length_truncates_output() {
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
                    "length": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\ttwo"));
        assert!(result.content.contains("\tthree"));
        assert!(!result.content.contains("\tfour"));
        assert!(result.content.contains(&format!(
            "File truncated, 2 lines remaining, next line tag {}",
            head.file.content[3].tag
        )));
        assert_eq!(result.metadata.as_ref().unwrap()["selected_count"], 2);
    }

    #[tokio::test]
    async fn xread_requires_start_tag_for_windowing() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xread-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let tool = XReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": "missing.txt",
                    "before": 2
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            "Read requires `start_tag` when `end_tag`, `length`, `before`, or `after` is provided."
        );
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
