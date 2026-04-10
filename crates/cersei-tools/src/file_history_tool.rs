//! FileHistory tool: exposes file revision history to the AI.
//!
//! Actions: list, revisions, get_revision, diff, revert, restore.

use super::*;
use crate::file_history::FileHistory;
use serde::Deserialize;
use std::sync::Arc;

pub struct FileHistoryTool;

#[async_trait]
impl Tool for FileHistoryTool {
    fn name(&self) -> &str {
        "FileHistory"
    }

    fn description(&self) -> &str {
        "Interact with the session's file revision history. Every time a file is edited or \
         written, a snapshot of the previous content is saved as a numbered revision. Use this \
         tool to list tracked files, inspect revisions, view diffs, revert to earlier versions, \
         or restore a revision's content into the current file.\n\
         \n\
         Actions:\n\
         - `list` — list all tracked files with read/write/edit counts and revision counts.\n\
         - `revisions` — list all revisions for a specific file (requires `file_path`).\n\
         - `get_revision` — get the full content of a revision (requires `file_path` + `revision`).\n\
         - `diff` — show a unified diff. Provide `file_path` and at least one of `from_revision` \
           or `to_revision`. Omitting `to_revision` diffs against the current file on disk. \
           Omitting `from_revision` diffs revision 1 against `to_revision`.\n\
         - `revert` — restore a file to a specific revision's content. The current content is \
           snapshotted first, so the revert itself can be reverted (requires `file_path` + `revision`).\n\
         - `restore` — same as revert (alias)."
    }

    fn permission_level(&self) -> PermissionLevel {
        // revert/restore mutate files, so this needs Write
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
                    "enum": ["list", "revisions", "get_revision", "diff", "revert", "restore"],
                    "description": "The action to perform"
                },
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file (required for all actions except `list`)"
                },
                "revision": {
                    "type": "integer",
                    "description": "Revision number (for get_revision, revert, restore)"
                },
                "from_revision": {
                    "type": "integer",
                    "description": "Start revision for diff (defaults to 1 if omitted)"
                },
                "to_revision": {
                    "type": "integer",
                    "description": "End revision for diff (omit to diff against current file on disk)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            action: String,
            file_path: Option<String>,
            revision: Option<u32>,
            from_revision: Option<u32>,
            to_revision: Option<u32>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let history: Arc<FileHistory> = match ctx.extensions.get::<FileHistory>() {
            Some(h) => h,
            None => {
                return ToolResult::error(
                    "FileHistory not available in this session. \
                     It must be inserted into Extensions before use.",
                )
            }
        };

        match input.action.as_str() {
            "list" => action_list(&history),
            "revisions" => {
                let path = match require_path(&input.file_path) { Ok(p) => p, Err(e) => return e };
                action_revisions(&history, &path)
            }
            "get_revision" => {
                let path = match require_path(&input.file_path) { Ok(p) => p, Err(e) => return e };
                let rev = match require_revision(input.revision) { Ok(r) => r, Err(e) => return e };
                action_get_revision(&history, &path, rev)
            }
            "diff" => {
                let path = match require_path(&input.file_path) { Ok(p) => p, Err(e) => return e };
                action_diff(&history, &path, input.from_revision, input.to_revision).await
            }
            "revert" | "restore" => {
                let path = match require_path(&input.file_path) { Ok(p) => p, Err(e) => return e };
                let rev = match require_revision(input.revision) { Ok(r) => r, Err(e) => return e };
                action_revert(&history, &path, rev).await
            }
            other => ToolResult::error(format!(
                "Unknown action: `{}`. Valid actions: list, revisions, get_revision, diff, revert, restore",
                other
            )),
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn require_path(file_path: &Option<String>) -> std::result::Result<std::path::PathBuf, ToolResult> {
    file_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| ToolResult::error("`file_path` is required for this action"))
}

fn require_revision(revision: Option<u32>) -> std::result::Result<u32, ToolResult> {
    revision.ok_or_else(|| ToolResult::error("`revision` is required for this action"))
}

// ─── Actions ────────────────────────────────────────────────────────────────

fn action_list(history: &FileHistory) -> ToolResult {
    let files = history.list_files();
    if files.is_empty() {
        return ToolResult::success("No files tracked in this session yet.");
    }

    let mut out = String::from("Tracked files:\n\n");
    for f in &files {
        out.push_str(&format!(
            "  {} — reads: {}, writes: {}, edits: {}, revisions: {}\n",
            f.path.display(),
            f.read_count,
            f.write_count,
            f.edit_count,
            f.revision_count,
        ));
    }
    ToolResult::success(out)
}

fn action_revisions(history: &FileHistory, path: &std::path::PathBuf) -> ToolResult {
    match history.get_revisions(path) {
        Some(revs) if revs.is_empty() => ToolResult::success(format!(
            "File {} is tracked but has no revisions (only reads so far).",
            path.display()
        )),
        Some(revs) => {
            let mut out = format!("Revisions for {}:\n\n", path.display());
            for r in &revs {
                out.push_str(&format!(
                    "  rev {} — op: {}, size: {} bytes, timestamp: {}\n",
                    r.number, r.operation, r.size_bytes, r.timestamp,
                ));
            }
            ToolResult::success(out)
        }
        None => ToolResult::error(format!("File {} is not tracked in this session.", path.display())),
    }
}

fn action_get_revision(history: &FileHistory, path: &std::path::PathBuf, revision: u32) -> ToolResult {
    match history.get_revision_content(path, revision) {
        Some(content) => ToolResult::success(content),
        None => ToolResult::error(format!(
            "Revision {} not found for {}. Use action `revisions` to see available revisions.",
            revision,
            path.display()
        )),
    }
}

async fn action_diff(
    history: &FileHistory,
    path: &std::path::PathBuf,
    from_revision: Option<u32>,
    to_revision: Option<u32>,
) -> ToolResult {
    match (from_revision, to_revision) {
        (Some(from), Some(to)) => {
            // Diff between two stored revisions
            match history.diff_two_revisions(path, from, to) {
                Some(diff) if diff.contains("@@") => ToolResult::success(diff),
                Some(_) => ToolResult::success("No differences between the two revisions."),
                None => ToolResult::error(format!(
                    "Could not diff rev {} and rev {} for {}. Check that both revisions exist.",
                    from, to, path.display()
                )),
            }
        }
        (Some(from), None) => {
            // Diff from a revision to current file on disk
            match tokio::fs::read_to_string(path).await {
                Ok(current) => match history.diff_revisions(path, from, &current, "current") {
                    Some(diff) if diff.contains("@@") => ToolResult::success(diff),
                    Some(_) => ToolResult::success("No differences between the revision and current file."),
                    None => ToolResult::error(format!(
                        "Revision {} not found for {}.",
                        from, path.display()
                    )),
                },
                Err(e) => ToolResult::error(format!("Failed to read current file: {}", e)),
            }
        }
        (None, Some(to)) => {
            // Diff from revision 1 to the specified revision
            match history.diff_two_revisions(path, 1, to) {
                Some(diff) if diff.contains("@@") => ToolResult::success(diff),
                Some(_) => ToolResult::success("No differences between rev 1 and the specified revision."),
                None => ToolResult::error(format!(
                    "Could not diff rev 1 and rev {} for {}. Check that both revisions exist.",
                    to, path.display()
                )),
            }
        }
        (None, None) => {
            // Diff latest revision vs current file on disk
            let rev_count = history.revision_count(path);
            if rev_count == 0 {
                return ToolResult::error(format!(
                    "No revisions for {}. Nothing to diff.",
                    path.display()
                ));
            }
            match tokio::fs::read_to_string(path).await {
                Ok(current) => {
                    match history.diff_revisions(path, rev_count, &current, "current") {
                        Some(diff) if diff.contains("@@") => ToolResult::success(diff),
                        Some(_) => ToolResult::success(
                            "No differences between the latest revision and current file.",
                        ),
                        None => ToolResult::error("Internal error: revision not found."),
                    }
                }
                Err(e) => ToolResult::error(format!("Failed to read current file: {}", e)),
            }
        }
    }
}

// Note: action_revert snapshots current content before writing, so the revert
// itself becomes a new revision that can be undone.

async fn action_revert(
    history: &FileHistory,
    path: &std::path::PathBuf,
    revision: u32,
) -> ToolResult {
    // Get the target revision content
    let target_content = match history.get_revision_content(path, revision) {
        Some(c) => c,
        None => {
            return ToolResult::error(format!(
                "Revision {} not found for {}. Use action `revisions` to see available revisions.",
                revision,
                path.display()
            ))
        }
    };

    // Snapshot the current content before reverting (so the revert can itself be reverted)
    match tokio::fs::read_to_string(path).await {
        Ok(current) => {
            history.snapshot_before_write(path, &current, "revert");
        }
        Err(_) => {
            // File may not exist on disk yet; that's OK for revert
        }
    }

    // Write the revision content to disk
    match tokio::fs::write(path, &target_content).await {
        Ok(()) => ToolResult::success(format!(
            "Reverted {} to revision {}. The previous content was saved as a new revision \
             (use `revisions` to see it), so this revert can itself be undone.",
            path.display(),
            revision
        )),
        Err(e) => ToolResult::error(format!("Failed to write file: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_history::FileHistory;
    use tempfile::NamedTempFile;
    use std::io::Write as IoWrite;

    /// Create a ToolContext with FileHistory in Extensions.
    /// Returns the context. Use `ctx.extensions.get::<FileHistory>()` to access the history.
    fn make_ctx_with_history() -> ToolContext {
        let ctx = ToolContext::default();
        ctx.extensions.insert(FileHistory::new());
        ctx
    }

    fn get_history(ctx: &ToolContext) -> Arc<FileHistory> {
        ctx.extensions.get::<FileHistory>().unwrap()
    }

    fn json_input(action: &str, extra: serde_json::Value) -> Value {
        let mut map = extra.as_object().cloned().unwrap_or_default();
        map.insert("action".to_string(), serde_json::json!(action));
        Value::Object(map)
    }

    #[tokio::test]
    async fn test_list_empty() {
        let ctx = make_ctx_with_history();
        let tool = FileHistoryTool;
        let result = tool.execute(json_input("list", serde_json::json!({})), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("No files tracked"));
    }

    #[tokio::test]
    async fn test_list_with_files() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);
        history.record_read(&PathBuf::from("/tmp/a.rs"));
        history.snapshot_before_write(&PathBuf::from("/tmp/b.rs"), "content", "edit");

        let tool = FileHistoryTool;
        let result = tool.execute(json_input("list", serde_json::json!({})), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("/tmp/a.rs"));
        assert!(result.content.contains("/tmp/b.rs"));
    }

    #[tokio::test]
    async fn test_no_history_in_extensions() {
        let ctx = ToolContext::default();
        let tool = FileHistoryTool;
        let result = tool.execute(json_input("list", serde_json::json!({})), &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("not available"));
    }

    #[tokio::test]
    async fn test_revisions_untracked_file() {
        let ctx = make_ctx_with_history();
        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("revisions", serde_json::json!({"file_path": "/no/such/file.rs"})),
            &ctx,
        ).await;
        assert!(result.is_error);
        assert!(result.content.contains("not tracked"));
    }

    #[tokio::test]
    async fn test_revisions_with_data() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);
        let path = PathBuf::from("/tmp/test.rs");
        history.snapshot_before_write(&path, "v1", "write");
        history.snapshot_before_write(&path, "v2", "edit");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("revisions", serde_json::json!({"file_path": "/tmp/test.rs"})),
            &ctx,
        ).await;
        assert!(!result.is_error);
        assert!(result.content.contains("rev 1"));
        assert!(result.content.contains("rev 2"));
    }

    #[tokio::test]
    async fn test_get_revision() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);
        let path = PathBuf::from("/tmp/test.rs");
        history.snapshot_before_write(&path, "hello world", "write");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("get_revision", serde_json::json!({"file_path": "/tmp/test.rs", "revision": 1})),
            &ctx,
        ).await;
        assert!(!result.is_error);
        assert_eq!(result.content, "hello world");
    }

    #[tokio::test]
    async fn test_get_revision_not_found() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);
        history.snapshot_before_write(&PathBuf::from("/tmp/test.rs"), "v1", "write");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("get_revision", serde_json::json!({"file_path": "/tmp/test.rs", "revision": 99})),
            &ctx,
        ).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_get_revision_missing_params() {
        let ctx = make_ctx_with_history();
        let tool = FileHistoryTool;

        // Missing file_path
        let result = tool.execute(
            json_input("get_revision", serde_json::json!({"revision": 1})),
            &ctx,
        ).await;
        assert!(result.is_error);
        assert!(result.content.contains("file_path"));

        // Missing revision
        let result = tool.execute(
            json_input("get_revision", serde_json::json!({"file_path": "/tmp/x.rs"})),
            &ctx,
        ).await;
        assert!(result.is_error);
        assert!(result.content.contains("revision"));
    }

    #[tokio::test]
    async fn test_diff_two_revisions() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);
        let path = PathBuf::from("/tmp/test.rs");
        history.snapshot_before_write(&path, "line1\n", "write");
        history.snapshot_before_write(&path, "line1\nline2\n", "edit");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("diff", serde_json::json!({
                "file_path": "/tmp/test.rs",
                "from_revision": 1,
                "to_revision": 2
            })),
            &ctx,
        ).await;
        assert!(!result.is_error);
        assert!(result.content.contains("+line2"));
    }

    #[tokio::test]
    async fn test_diff_revision_vs_current() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);

        let mut tmpfile = NamedTempFile::new().unwrap();
        write!(tmpfile, "current content\n").unwrap();
        let path = tmpfile.path().to_path_buf();

        history.snapshot_before_write(&path, "old content\n", "write");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("diff", serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "from_revision": 1
            })),
            &ctx,
        ).await;
        assert!(!result.is_error);
        assert!(result.content.contains("-old content"));
        assert!(result.content.contains("+current content"));
    }

    #[tokio::test]
    async fn test_diff_no_revisions() {
        let ctx = make_ctx_with_history();
        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("diff", serde_json::json!({"file_path": "/tmp/none.rs"})),
            &ctx,
        ).await;
        assert!(result.is_error);
        assert!(result.content.contains("No revisions"));
    }

    #[tokio::test]
    async fn test_revert() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);

        let mut tmpfile = NamedTempFile::new().unwrap();
        write!(tmpfile, "modified content").unwrap();
        let path = tmpfile.path().to_path_buf();

        history.snapshot_before_write(&path, "original content", "write");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("revert", serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "revision": 1
            })),
            &ctx,
        ).await;
        assert!(!result.is_error);
        assert!(result.content.contains("Reverted"));

        // Verify file content was restored
        let restored = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(restored, "original content");

        // Verify a new revert revision was created (the pre-revert snapshot)
        let revs = history.get_revisions(&path).unwrap();
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[1].operation, "revert");
    }

    #[tokio::test]
    async fn test_revert_nonexistent_revision() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);
        history.snapshot_before_write(&PathBuf::from("/tmp/x.rs"), "v1", "write");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("revert", serde_json::json!({
                "file_path": "/tmp/x.rs",
                "revision": 99
            })),
            &ctx,
        ).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_restore_alias() {
        let ctx = make_ctx_with_history();
        let history = get_history(&ctx);

        let mut tmpfile = NamedTempFile::new().unwrap();
        write!(tmpfile, "new").unwrap();
        let path = tmpfile.path().to_path_buf();

        history.snapshot_before_write(&path, "old", "write");

        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("restore", serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "revision": 1
            })),
            &ctx,
        ).await;
        assert!(!result.is_error);

        let restored = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(restored, "old");
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let ctx = make_ctx_with_history();
        let tool = FileHistoryTool;
        let result = tool.execute(
            json_input("explode", serde_json::json!({})),
            &ctx,
        ).await;
        assert!(result.is_error);
        assert!(result.content.contains("Unknown action"));
    }
}
