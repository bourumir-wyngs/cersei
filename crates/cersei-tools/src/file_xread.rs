//! Read tool: session-scoped tagged file reads backed by XFileStorage.

use super::*;
use crate::xfile_storage::{ensure_loaded, resolve_xfile_path, XFile, XLine};
use serde::Deserialize;

pub struct XReadTool;

/// Public alias preserved for downstream imports.
pub type FileXReadTool = XReadTool;

#[derive(Debug, Clone, Deserialize)]
pub struct XReadRequest {
    pub file_path: String,
    #[serde(default)]
    pub start_tag: Option<String>,
    #[serde(default)]
    pub end_tag: Option<String>,
    #[serde(default)]
    pub length: Option<usize>,
}

#[async_trait]
impl Tool for XReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read the latest session-scoped XFileStorage revision of a file. If the file is not yet in XFileStorage for this session, Read loads it from disk first. Output is plain text with one result line per file line in the form `<tag>\\t<content>`, where `tag` is the stable unique line identifier to use with Edit. If no range fields are provided, Read returns the whole file. If only `start_tag` is provided, Read returns from that tag through end of file. `end_tag` is inclusive. Metadata includes `current_version`, `line_count`, and `selected_count`."
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
                    "description": "Optional starting tag from previous Read or Grep output. If provided without `end_tag` or `length`, Read returns from this tag through end of file."
                },
                "end_tag": {
                    "type": "string",
                    "description": "Optional inclusive ending tag from previous Read or Grep output. Use with `start_tag`."
                },
                "length": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional number of lines to return starting at `start_tag`, including the line identified by `start_tag`."
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

        if req.end_tag.is_some() && req.length.is_some() {
            return ToolResult::error("Read accepts either `end_tag` or `length`, not both.");
        }
        if req.start_tag.is_none() && (req.end_tag.is_some() || req.length.is_some()) {
            return ToolResult::error(
                "Read requires `start_tag` when `end_tag` or `length` is provided.",
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
            req.length,
        ) {
            Ok(lines) => lines,
            Err(err) => return ToolResult::error(err),
        };

        let content = selected
            .iter()
            .map(|line| format!("{:>6}\t{}", line.tag, line.content))
            .collect::<Vec<_>>()
            .join("\n");

        ToolResult::success(content).with_metadata(serde_json::json!({
            "file_path": head.file.path.display().to_string(),
            "current_version": head.current_version,
            "line_count": head.file.content.len(),
            "selected_count": selected.len(),
        }))
    }
}

fn select_lines<'a>(
    file: &'a XFile,
    start_tag: Option<&str>,
    end_tag: Option<&str>,
    length: Option<usize>,
) -> std::result::Result<Vec<&'a XLine>, String> {
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

        if let Some(end_tag) = end_tag {
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
            Ok(file.content[start_idx..=end_idx].iter().collect())
        } else if let Some(length) = length {
            Ok(file.content.iter().skip(start_idx).take(length).collect())
        } else {
            Ok(file.content.iter().skip(start_idx).collect())
        }
    } else {
        Ok(file.content.iter().collect())
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
            "Read requires `start_tag` when `end_tag` or `length` is provided."
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
                    "end_tag": head.file.content[1].tag
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
