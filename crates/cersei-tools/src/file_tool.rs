//! File tool: copy, move, and delete files through XFileStorage.

use super::*;
use crate::xfile_storage::{
    apply_file_to_disk, apply_file_transition_to_disk, copy_tracked_file, ensure_loaded,
    move_tracked_file, record_disk_state, resolve_xfile_path, store_deleted_file,
    sync_if_disk_changed, try_get_head, xfile_session_id,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub struct FileTool;

#[derive(Debug, Clone, Deserialize)]
pub struct FileRequest {
    pub action: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub destination_path: Option<String>,
    #[serde(default)]
    pub create_parents: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSuccess {
    pub ok: bool,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_path: Option<String>,
    pub current_version: String,
    pub revision_count: usize,
    pub line_count: usize,
}

#[async_trait]
impl Tool for FileTool {
    fn name(&self) -> &str {
        "File"
    }

    fn description(&self) -> &str {
        "Copy, move, or delete files through session-scoped XFileStorage. `copy` requires `source_path` and `destination_path`, keeps the source unchanged, and assigns fresh line tags to the copy. `move` requires `source_path` and `destination_path`, preserves all line tags, and transfers the tracked file history to the destination path. `delete` requires `file_path` and records an absent-file revision so Revert/FileHistory can restore it later. Copy and move require a missing destination path."
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
                "action": {
                    "type": "string",
                    "enum": ["copy", "move", "delete"],
                    "description": "File action to perform."
                },
                "file_path": {
                    "type": "string",
                    "description": "Target path for `delete`. Absolute paths and workspace-relative paths are accepted."
                },
                "source_path": {
                    "type": "string",
                    "description": "Source path for `copy` or `move`. Absolute paths and workspace-relative paths are accepted."
                },
                "destination_path": {
                    "type": "string",
                    "description": "Destination path for `copy` or `move`. The destination must not already exist or be tracked in XFileStorage."
                },
                "create_parents": {
                    "type": "boolean",
                    "description": "Optional flag controlling parent-directory creation for `copy` and `move`. Defaults to true. If false, the action fails when the destination parent directory does not exist."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: FileRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        match req.action.as_str() {
            "copy" => execute_copy(req, ctx).await,
            "move" => execute_move(req, ctx).await,
            "delete" => execute_delete(req, ctx).await,
            other => ToolResult::error(format!(
                "Unknown action: `{}`. Valid actions: copy, move, delete",
                other
            )),
        }
    }
}

async fn execute_copy(req: FileRequest, ctx: &ToolContext) -> ToolResult {
    let storage_session_id = xfile_session_id(ctx);
    let (source, destination) = match require_source_and_destination(&req, ctx) {
        Ok(paths) => paths,
        Err(err) => return err,
    };
    if let Err(err) = prepare_parent_dirs(&destination, req.create_parents.unwrap_or(true)).await {
        return ToolResult::error(err);
    }
    if let Err(err) = ensure_destination_available(&storage_session_id, &destination) {
        return ToolResult::error(err);
    }

    if let Err(err) = load_and_sync_if_needed(&storage_session_id, &source).await {
        return ToolResult::error(err);
    }
    let head = match copy_tracked_file(&storage_session_id, &source, &destination) {
        Ok(head) => head,
        Err(err) => return ToolResult::error(err),
    };
    if let Err(err) = apply_file_to_disk(&head.file.path, &head.file).await {
        return ToolResult::error(err);
    }
    if let Err(err) = record_disk_state(&storage_session_id, &head.file.path) {
        return ToolResult::error(err);
    }

    success_payload("copy", None, Some(source), Some(destination), &head)
}

async fn execute_move(req: FileRequest, ctx: &ToolContext) -> ToolResult {
    let storage_session_id = xfile_session_id(ctx);
    let (source, destination) = match require_source_and_destination(&req, ctx) {
        Ok(paths) => paths,
        Err(err) => return err,
    };
    if let Err(err) = prepare_parent_dirs(&destination, req.create_parents.unwrap_or(true)).await {
        return ToolResult::error(err);
    }
    if let Err(err) = ensure_destination_available(&storage_session_id, &destination) {
        return ToolResult::error(err);
    }

    let current = match load_and_sync_if_needed(&storage_session_id, &source).await {
        Ok(head) => head,
        Err(err) => return ToolResult::error(err),
    };
    let head = match move_tracked_file(&storage_session_id, &source, &destination) {
        Ok(head) => head,
        Err(err) => return ToolResult::error(err),
    };
    if let Err(err) = apply_file_transition_to_disk(&current.file, &head.file).await {
        return ToolResult::error(err);
    }
    if let Err(err) = record_disk_state(&storage_session_id, &head.file.path) {
        return ToolResult::error(err);
    }

    success_payload("move", None, Some(source), Some(destination), &head)
}

async fn execute_delete(req: FileRequest, ctx: &ToolContext) -> ToolResult {
    let storage_session_id = xfile_session_id(ctx);
    let path = match req
        .file_path
        .as_deref()
        .map(|path| resolve_xfile_path(ctx, path))
    {
        Some(path) => path,
        None => return ToolResult::error("`file_path` is required for action `delete`"),
    };
    let current = match load_and_sync_if_needed(&storage_session_id, &path).await {
        Ok(head) => head,
        Err(err) => return ToolResult::error(err),
    };
    if !current.file.exists {
        return ToolResult::error(format!("File is already absent: {}", path.display()));
    }

    let head = store_deleted_file(&storage_session_id, &path);
    if let Err(err) = apply_file_transition_to_disk(&current.file, &head.file).await {
        return ToolResult::error(err);
    }
    if let Err(err) = record_disk_state(&storage_session_id, &head.file.path) {
        return ToolResult::error(err);
    }

    success_payload("delete", Some(path), None, None, &head)
}

fn success_payload(
    action: &str,
    file_path: Option<std::path::PathBuf>,
    source_path: Option<std::path::PathBuf>,
    destination_path: Option<std::path::PathBuf>,
    head: &crate::xfile_storage::XFileHead,
) -> ToolResult {
    let payload = FileSuccess {
        ok: true,
        action: action.to_string(),
        file_path: file_path.map(|path| path.display().to_string()),
        source_path: source_path.map(|path| path.display().to_string()),
        destination_path: destination_path.map(|path| path.display().to_string()),
        current_version: head.current_version.clone(),
        revision_count: head.revision_count,
        line_count: head.file.content.len(),
    };

    ToolResult::success(serde_json::to_string_pretty(&payload).unwrap_or_default()).with_metadata(
        serde_json::json!({
            "action": payload.action,
            "file_path": payload.file_path,
            "source_path": payload.source_path,
            "destination_path": payload.destination_path,
            "current_version": payload.current_version,
            "revision_count": payload.revision_count,
            "line_count": payload.line_count,
        }),
    )
}

fn require_source_and_destination(
    req: &FileRequest,
    ctx: &ToolContext,
) -> std::result::Result<(std::path::PathBuf, std::path::PathBuf), ToolResult> {
    let source = req
        .source_path
        .as_deref()
        .map(|path| resolve_xfile_path(ctx, path))
        .ok_or_else(|| ToolResult::error("`source_path` is required for this action"))?;
    let destination = req
        .destination_path
        .as_deref()
        .map(|path| resolve_xfile_path(ctx, path))
        .ok_or_else(|| ToolResult::error("`destination_path` is required for this action"))?;
    if source == destination {
        return Err(ToolResult::error(
            "Source and destination must be different for this action.",
        ));
    }
    Ok((source, destination))
}

fn ensure_destination_available(
    session_id: &str,
    destination: &Path,
) -> std::result::Result<(), String> {
    if destination.exists() {
        return Err(format!(
            "Destination already exists on disk: {}",
            destination.display()
        ));
    }
    if try_get_head(session_id, destination).is_some() {
        return Err(format!(
            "Destination is already tracked in XFileStorage: {}",
            destination.display()
        ));
    }
    Ok(())
}

async fn load_and_sync_if_needed(
    session_id: &str,
    path: &Path,
) -> std::result::Result<crate::xfile_storage::XFileHead, String> {
    if path.exists() {
        ensure_loaded(session_id, path).await?;
    } else if try_get_head(session_id, path).is_none() {
        return Err(format!(
            "File does not exist on disk and is not tracked in XFileStorage: {}",
            path.display()
        ));
    }

    sync_if_disk_changed(session_id, path).await?;
    try_get_head(session_id, path).ok_or_else(|| {
        format!(
            "File is not loaded in XFileStorage for this session: {}",
            path.display()
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_history_tool::FileHistoryTool;
    use crate::file_xrevert::XRevertTool;
    use crate::file_xwrite::XWriteTool;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{clear_session_xfile_storage, list_revisions, try_get_head};
    use serde_json::json;
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

    async fn xwrite(ctx: &ToolContext, file_path: &str, content: &str) {
        let tool = XWriteTool;
        let result = tool
            .execute(
                json!({
                    "file_path": file_path,
                    "content": content,
                }),
                ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[test]
    fn filesystem_toolset_includes_file_tool() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "File"));
    }

    #[tokio::test]
    async fn copy_assigns_new_tags_and_records_history() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("file-tool-copy-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let ctx = test_ctx(tmp.path(), &session_id);
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");

        xwrite(&ctx, &src.display().to_string(), "alpha\nbeta\n").await;

        let src_head = try_get_head(&session_id, &src).unwrap();
        let tool = FileTool;
        let result = tool
            .execute(
                json!({
                    "action": "copy",
                    "source_path": src.display().to_string(),
                    "destination_path": dst.display().to_string(),
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            tokio::fs::read_to_string(&dst).await.unwrap(),
            "alpha\nbeta\n"
        );

        let dst_head = try_get_head(&session_id, &dst).unwrap();
        assert_ne!(src_head.file.content[0].tag, dst_head.file.content[0].tag);
        assert_ne!(src_head.file.content[1].tag, dst_head.file.content[1].tag);

        let revisions = list_revisions(&session_id, &dst).unwrap();
        assert_eq!(revisions.len(), 2);
        assert!(!revisions[0].file.exists);
        assert_eq!(
            revisions[1].metadata.as_ref().unwrap().operation.as_deref(),
            Some("copy")
        );

        let history = FileHistoryTool;
        let revisions_view = history
            .execute(
                json!({
                    "action": "revisions",
                    "file_path": dst.display().to_string(),
                }),
                &ctx,
            )
            .await;
        assert!(!revisions_view.is_error, "{}", revisions_view.content);
        assert!(revisions_view.content.contains("op: copy"));
    }

    #[tokio::test]
    async fn move_preserves_tags_transfers_history_and_can_be_reverted() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("file-tool-move-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let ctx = test_ctx(tmp.path(), &session_id);
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("moved/dst.txt");

        xwrite(&ctx, &src.display().to_string(), "alpha\nbeta\n").await;
        let src_head = try_get_head(&session_id, &src).unwrap();
        let tool = FileTool;
        let result = tool
            .execute(
                json!({
                    "action": "move",
                    "source_path": src.display().to_string(),
                    "destination_path": dst.display().to_string(),
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(!src.exists());
        assert_eq!(
            tokio::fs::read_to_string(&dst).await.unwrap(),
            "alpha\nbeta\n"
        );

        assert!(try_get_head(&session_id, &src).is_none());
        let dst_head = try_get_head(&session_id, &dst).unwrap();
        assert_eq!(src_head.file.content[0].tag, dst_head.file.content[0].tag);
        assert_eq!(src_head.file.content[1].tag, dst_head.file.content[1].tag);

        let revisions = list_revisions(&session_id, &dst).unwrap();
        assert_eq!(
            revisions
                .last()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .operation
                .as_deref(),
            Some("move")
        );

        let revert_tool = XRevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": dst.display().to_string() }), &ctx)
            .await;
        assert!(!revert.is_error, "{}", revert.content);
        assert!(src.exists());
        assert!(!dst.exists());
        let restored = try_get_head(&session_id, &src).unwrap();
        assert_eq!(restored.file.content[0].tag, src_head.file.content[0].tag);
    }

    #[tokio::test]
    async fn delete_creates_absent_revision_and_revert_restores_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("file-tool-delete-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let ctx = test_ctx(tmp.path(), &session_id);
        let path = tmp.path().join("sample.txt");

        xwrite(&ctx, &path.display().to_string(), "alpha\n").await;

        let tool = FileTool;
        let result = tool
            .execute(
                json!({
                    "action": "delete",
                    "file_path": path.display().to_string(),
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(!path.exists());
        let head = try_get_head(&session_id, &path).unwrap();
        assert!(!head.file.exists);

        let revert_tool = XRevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": path.display().to_string() }), &ctx)
            .await;
        assert!(!revert.is_error, "{}", revert.content);
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "alpha\n");
    }
}
