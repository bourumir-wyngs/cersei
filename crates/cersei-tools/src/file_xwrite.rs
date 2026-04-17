//! Write tool: session-scoped tagged writes backed by XFileStorage.

use super::*;
use crate::xfile_storage::{
    ensure_loaded, record_disk_state, resolve_xfile_path, store_written_text, sync_if_disk_changed,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub struct XWriteTool;

/// Public alias preserved for downstream imports.
pub type FileXWriteTool = XWriteTool;

#[derive(Debug, Clone, Deserialize)]
pub struct XWriteRequest {
    pub file_path: String,
    pub content: String,
    #[serde(default)]
    pub create_parents: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XWriteSuccess {
    pub ok: bool,
    pub file_path: String,
    pub current_version: String,
    pub revision_count: usize,
    pub line_count: usize,
}

#[async_trait]
impl Tool for XWriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Write a full file through session-scoped XFileStorage. If the target file already exists, Write first loads the current disk contents into XFileStorage so the overwrite remains recoverable. It then replaces the file content, assigns fresh unique tags to every stored line, pushes a new XFileStorage revision, and flushes the rendered file to disk. Non-empty files are normalized to end with a trailing newline. Metadata includes `current_version`, `revision_count`, and `line_count`."
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
                    "description": "Path to the target file. Absolute paths and workspace-relative paths are accepted."
                },
                "content": {
                    "type": "string",
                    "description": "Full file content to write. Every stored line receives a fresh unique tag. Non-empty files are flushed with a trailing newline."
                },
                "create_parents": {
                    "type": "boolean",
                    "description": "Optional flag controlling parent-directory creation. Defaults to true. If false, Write fails when the parent directory does not already exist."
                }
            },
            "required": ["file_path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: XWriteRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        let path = resolve_xfile_path(ctx, &req.file_path);
        if let Err(err) = prepare_parent_dirs(&path, req.create_parents.unwrap_or(true)).await {
            return ToolResult::error(err);
        }
        if let Err(err) = track_existing_file_before_write(&ctx.session_id, &path).await {
            return ToolResult::error(err);
        }

        let head = store_written_text(&ctx.session_id, &path, &req.content);
        if let Err(err) = tokio::fs::write(&path, &head.rendered_content).await {
            return ToolResult::error(format!("Failed to write file: {}", err));
        }
        if let Err(err) = record_disk_state(&ctx.session_id, &path) {
            return ToolResult::error(err);
        }

        let payload = XWriteSuccess {
            ok: true,
            file_path: path.display().to_string(),
            current_version: head.current_version.clone(),
            revision_count: head.revision_count,
            line_count: head.file.content.len(),
        };

        ToolResult::success(serde_json::to_string_pretty(&payload).unwrap_or_default())
            .with_metadata(serde_json::json!({
                "file_path": payload.file_path,
                "current_version": payload.current_version,
                "revision_count": payload.revision_count,
                "line_count": payload.line_count,
            }))
    }
}

async fn prepare_parent_dirs(path: &Path, create_parents: bool) -> std::result::Result<(), String> {
    if let Some(parent) = path.parent() {
        if parent.exists() {
            return Ok(());
        }
        if !create_parents {
            return Err(format!(
                "Parent directory does not exist for {} and `create_parents` is false.",
                path.display()
            ));
        }
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directories: {}", e))?;
    }
    Ok(())
}

async fn track_existing_file_before_write(
    session_id: &str,
    path: &Path,
) -> std::result::Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    ensure_loaded(session_id, path).await?;
    sync_if_disk_changed(session_id, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{
        clear_session_xfile_storage, list_revisions, render_file, try_get_head,
    };
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
    fn xwrite_schema_exposes_write_inputs() {
        let tool = XWriteTool;
        let schema = tool.input_schema();

        assert_eq!(schema["properties"]["file_path"]["type"], "string");
        assert_eq!(schema["properties"]["content"]["type"], "string");
        assert_eq!(schema["properties"]["create_parents"]["type"], "boolean");
    }

    #[test]
    fn filesystem_toolset_includes_xwrite() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "Write"));
    }

    #[tokio::test]
    async fn xwrite_flushes_and_tracks_revisioned_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xwrite-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("file.txt");
        let tool = XWriteTool;

        let first = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "content": "hello\nworld\n"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;
        assert!(!first.is_error, "{}", first.content);

        let second = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "content": "updated\nworld\n"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;
        assert!(!second.is_error, "{}", second.content);

        let disk = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(disk, "updated\nworld\n");

        let head = try_get_head(&session_id, &path).unwrap();
        assert_eq!(head.revision_count, 2);
        assert_eq!(head.rendered_content, disk);
    }

    #[tokio::test]
    async fn xwrite_tracks_existing_file_before_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xwrite-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("file.txt");
        tokio::fs::write(&path, "original\n").await.unwrap();
        let tool = XWriteTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "content": "updated\n"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let revisions = list_revisions(&session_id, &path).unwrap();
        assert_eq!(revisions.len(), 2);
        assert_eq!(render_file(&revisions[0].file), "original\n");
        assert_eq!(render_file(&revisions[1].file), "updated\n");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "updated\n");
    }

    #[tokio::test]
    async fn xwrite_syncs_external_disk_change_before_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xwrite-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("file.txt");
        let tool = XWriteTool;

        let first = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "content": "first\n"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;
        assert!(!first.is_error, "{}", first.content);

        tokio::fs::write(&path, "external\n").await.unwrap();

        let second = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "content": "final\n"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;
        assert!(!second.is_error, "{}", second.content);

        let revisions = list_revisions(&session_id, &path).unwrap();
        assert_eq!(revisions.len(), 3);
        assert_eq!(render_file(&revisions[0].file), "first\n");
        assert_eq!(render_file(&revisions[1].file), "external\n");
        assert_eq!(render_file(&revisions[2].file), "final\n");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "final\n");
    }

    #[tokio::test]
    async fn xwrite_creates_parent_dirs_and_normalizes_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xwrite-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("nested/deep/file.txt");
        let tool = XWriteTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "content": "hello"
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let disk = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(disk, "hello\n");
        assert_eq!(result.metadata.as_ref().unwrap()["line_count"], 1);
    }

    #[tokio::test]
    async fn xwrite_respects_create_parents_false() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xwrite-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("missing/dir/file.txt");
        let tool = XWriteTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "content": "hello",
                    "create_parents": false
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Parent directory does not exist"));
        assert!(!path.exists());
        assert!(try_get_head(&session_id, &path).is_none());
    }
}
