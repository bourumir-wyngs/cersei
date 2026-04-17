//! Revert tool: restore the previous XFileStorage revision of a tracked file.

use super::*;
use crate::file_history::unified_diff;
use crate::xfile_storage::{
    discard_head_revision, list_revisions, record_disk_state, render_file, resolve_xfile_path,
};
use serde::Deserialize;

pub struct XRevertTool;

#[async_trait]
impl Tool for XRevertTool {
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
                return ToolResult::error(
                    "file_path is required for XFileStorage-backed revert.".to_string(),
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_xedit::XEditTool;
    use crate::file_xwrite::XWriteTool;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::try_get_head;
    use serde_json::json;
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
    fn filesystem_toolset_includes_revert() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "Revert"));
    }

    #[tokio::test]
    async fn revert_restores_previous_xwrite_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xrevert-test-{}", Uuid::new_v4());
        let ctx = test_ctx(tmp.path(), &session_id);
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
        let path = tmp.path().join("sample.txt");
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "hello there\n"
        );

        let revert_tool = XRevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": "sample.txt" }), &ctx)
            .await;

        assert!(!revert.is_error, "{}", revert.content);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "hello world\n"
        );
    }

    #[tokio::test]
    async fn revert_restores_previous_xedit_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xrevert-test-{}", Uuid::new_v4());
        let ctx = test_ctx(tmp.path(), &session_id);
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
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "alpha\nBETA\n"
        );

        let revert_tool = XRevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": "sample.txt" }), &ctx)
            .await;

        assert!(!revert.is_error, "{}", revert.content);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "alpha\nbeta\n"
        );
    }

    #[tokio::test]
    async fn revert_rejects_untracked_non_xstorage_files() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xrevert-test-{}", Uuid::new_v4());
        let ctx = test_ctx(tmp.path(), &session_id);
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "hello world\n").await.unwrap();

        let revert_tool = XRevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": "sample.txt" }), &ctx)
            .await;

        assert!(revert.is_error);
        assert!(revert
            .content
            .contains("File is not loaded in XFileStorage"));
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "hello world\n"
        );
    }
}
