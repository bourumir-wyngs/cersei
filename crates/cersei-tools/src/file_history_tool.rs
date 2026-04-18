//! FileHistory tool: exposes XFileStorage revision history to the AI.
//!
//! Actions: list, revisions, get_revision, diff, revert, restore, checkpoint, rollback.

use super::*;
use crate::xfile_storage::{
    apply_file_transition_to_disk, create_checkpoint, diff_against_checkpoint, diff_files,
    file_state, files_differ, get_revision, list_revisions, list_tracked_files, record_disk_state,
    render_file, resolve_xfile_path, restore_revision, rollback_to_checkpoint, xfile_session_id,
    XFileRevisionMetadata,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct FileHistoryTool;
pub struct ReadOnlyFileHistoryTool;

#[async_trait]
impl Tool for FileHistoryTool {
    fn name(&self) -> &str {
        "FileHistory"
    }

    fn description(&self) -> &str {
        full_description()
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        full_input_schema()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        execute_impl(input, ctx, false).await
    }
}

#[async_trait]
impl Tool for ReadOnlyFileHistoryTool {
    fn name(&self) -> &str {
        "FileHistory"
    }

    fn description(&self) -> &str {
        read_only_description()
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        read_only_input_schema()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        execute_impl(input, ctx, true).await
    }
}

fn full_description() -> &'static str {
    "Inspect session-scoped XFileStorage revision history for files loaded by Read, Write, Edit, or matching Grep results. History is limited to the retained XFileStorage revisions for this session. \
     Actions:\n\
     - `list` — list tracked XFileStorage files with current revision, presence state, and line counts.\n\
     - `revisions` — list retained revisions for a specific file (requires `file_path`). Revisions can represent either a present file or an absent file and may include operation metadata such as copy or move details.\n\
     - `get_revision` — get the full rendered content of a stored revision (requires `file_path` + `revision`).\n\
     - `diff` — with `file_path`, show a unified diff between retained revisions. Omitting `to_revision` diffs against the current XFileStorage head. \
       Omitting `from_revision` defaults to the earliest retained revision. Omitting both diffs the previous revision against the current head. \
       Without `file_path`, show a combined diff between the current session state and the latest checkpoint. If no explicit checkpoint exists yet, the implicit session-start baseline is used.\n\
     - `revert` — restore a file to a specific retained revision by cloning that revision into a new XFileStorage head and applying it to disk (requires `file_path` + `revision`). If the target revision is absent, the file is deleted.\n\
     - `restore` — same as revert (alias).\n\
     - `checkpoint` — save the current retained head revision of every tracked file in this session.\n\
     - `rollback` — destructively roll tracked files back to the saved checkpoint. If no explicit checkpoint exists yet, rollback uses each tracked file's earliest retained session revision as the baseline."
}

fn read_only_description() -> &'static str {
    "Inspect XFileStorage revision history for files loaded by Read, Write, Edit, or matching Grep results. This reviewer variant is read-only. \
     Actions:\n\
     - `list` — list tracked XFileStorage files with current revision, presence state, and line counts.\n\
     - `revisions` — list retained revisions for a specific file (requires `file_path`).\n\
     - `get_revision` — get the full rendered content of a stored revision (requires `file_path` + `revision`).\n\
     - `diff` — with `file_path`, show a unified diff between retained revisions. Omitting `to_revision` diffs against the current XFileStorage head. \
       Omitting `from_revision` defaults to the earliest retained revision. Omitting both diffs the previous revision against the current head. \
       Without `file_path`, show a combined diff between the current session state and the latest checkpoint. If no explicit checkpoint exists yet, the implicit session-start baseline is used."
}

fn full_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "revisions", "get_revision", "diff", "revert", "restore", "checkpoint", "rollback"],
                "description": "The action to perform. `list`, `checkpoint`, and `rollback` need no extra fields. `diff` may omit `file_path` to compare the current session state against the latest checkpoint. `revisions` needs `file_path`. `get_revision`, `revert`, and `restore` need both `file_path` and `revision`."
            },
            "file_path": {
                "type": "string",
                "description": "Path to the tracked file. Required for `revisions`, `get_revision`, `revert`, and `restore`. Optional for `diff`: if omitted, `diff` compares the whole tracked session state to the latest checkpoint. Absolute paths and workspace-relative paths are accepted."
            },
            "revision": {
                "type": "integer",
                "description": "Revision number for `get_revision`, `revert`, or `restore`."
            },
            "from_revision": {
                "type": "integer",
                "description": "Starting revision for `diff`. If omitted and `to_revision` is provided, diff starts from the earliest retained revision. If both are omitted, diff compares the previous revision to the current head."
            },
            "to_revision": {
                "type": "integer",
                "description": "Ending revision for `diff`. If omitted, diff ends at the current head revision."
            }
        },
        "required": ["action"]
    })
}

fn read_only_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "revisions", "get_revision", "diff"],
                "description": "The action to perform. `revisions` needs `file_path`. `get_revision` needs `file_path` and `revision`. `diff` may omit `file_path` to compare the current session state against the latest checkpoint."
            },
            "file_path": {
                "type": "string",
                "description": "Path to the tracked file. Required for `revisions` and `get_revision`. Optional for `diff`: if omitted, `diff` compares the whole tracked session state to the latest checkpoint. Absolute paths and workspace-relative paths are accepted."
            },
            "revision": {
                "type": "integer",
                "description": "Revision number for `get_revision`."
            },
            "from_revision": {
                "type": "integer",
                "description": "Starting revision for `diff`. If omitted and `to_revision` is provided, diff starts from the earliest retained revision. If both are omitted, diff compares the previous revision to the current head."
            },
            "to_revision": {
                "type": "integer",
                "description": "Ending revision for `diff`. If omitted, diff ends at the current head revision."
            }
        },
        "required": ["action"]
    })
}

fn is_mutating_action(action: &str) -> bool {
    matches!(action, "revert" | "restore" | "checkpoint" | "rollback")
}

async fn execute_impl(input: Value, ctx: &ToolContext, read_only: bool) -> ToolResult {
    #[derive(Deserialize)]
    struct Input {
        action: String,
        file_path: Option<String>,
        revision: Option<usize>,
        from_revision: Option<usize>,
        to_revision: Option<usize>,
    }

    let input: Input = match serde_json::from_value(input) {
        Ok(i) => i,
        Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
    };
    let storage_session_id = xfile_session_id(ctx);

    if read_only && is_mutating_action(&input.action) {
        return ToolResult::error(
            "FileHistory is read-only in reviewer sessions. Allowed actions: list, revisions, get_revision, diff",
        );
    }

    match input.action.as_str() {
        "list" => action_list(&storage_session_id),
        "checkpoint" => action_checkpoint(&storage_session_id),
        "revisions" => {
            let path = match require_path(&input.file_path, ctx) {
                Ok(path) => path,
                Err(err) => return err,
            };
            action_revisions(&storage_session_id, &path)
        }
        "get_revision" => {
            let path = match require_path(&input.file_path, ctx) {
                Ok(path) => path,
                Err(err) => return err,
            };
            let revision = match require_revision(input.revision) {
                Ok(revision) => revision,
                Err(err) => return err,
            };
            action_get_revision(&storage_session_id, &path, revision)
        }
        "diff" => match &input.file_path {
            Some(path) => {
                let path = resolve_xfile_path(ctx, path);
                action_diff(
                    &storage_session_id,
                    &path,
                    input.from_revision,
                    input.to_revision,
                )
            }
            None => {
                if input.from_revision.is_some() || input.to_revision.is_some() {
                    return ToolResult::error(
                        "`from_revision` and `to_revision` require `file_path` for action `diff`",
                    );
                }
                action_checkpoint_diff(&storage_session_id)
            }
        },
        "revert" | "restore" => {
            let path = match require_path(&input.file_path, ctx) {
                Ok(path) => path,
                Err(err) => return err,
            };
            let revision = match require_revision(input.revision) {
                Ok(revision) => revision,
                Err(err) => return err,
            };
            action_revert(&storage_session_id, &path, revision).await
        }
        "rollback" => action_rollback(&storage_session_id).await,
        other => ToolResult::error(format!(
            "Unknown action: `{}`. Valid actions: list, revisions, get_revision, diff, revert, restore, checkpoint, rollback",
            other
        )),
    }
}

fn require_path(
    file_path: &Option<String>,
    ctx: &ToolContext,
) -> std::result::Result<PathBuf, ToolResult> {
    file_path
        .as_deref()
        .map(|path| resolve_xfile_path(ctx, path))
        .ok_or_else(|| ToolResult::error("`file_path` is required for this action"))
}

fn require_revision(revision: Option<usize>) -> std::result::Result<usize, ToolResult> {
    revision.ok_or_else(|| ToolResult::error("`revision` is required for this action"))
}

fn tracked_revisions(
    session_id: &str,
    path: &Path,
) -> std::result::Result<Vec<crate::xfile_storage::XFileRevision>, ToolResult> {
    match list_revisions(session_id, path) {
        Some(revisions) => Ok(revisions),
        None => Err(ToolResult::error(format!(
            "File {} is not tracked in XFileStorage for this session. Use action `list` to see tracked files.",
            path.display()
        ))),
    }
}

fn action_list(session_id: &str) -> ToolResult {
    let files = list_tracked_files(session_id);
    if files.is_empty() {
        return ToolResult::success("No XFileStorage-backed files tracked in this session yet.");
    }

    let mut out = String::from("Tracked XFileStorage files:\n\n");
    for file in files {
        out.push_str(&format!(
            "  {} — revisions: {}, current: rev {}, state: {}, lines: {}\n",
            file.path.display(),
            file.revision_count,
            file.current_revision,
            if file.exists { "present" } else { "absent" },
            file.line_count,
        ));
    }
    ToolResult::success(out)
}

fn action_checkpoint(session_id: &str) -> ToolResult {
    let summary = create_checkpoint(session_id);
    if summary.tracked_files == 0 {
        return ToolResult::success(
            "Saved an empty FileHistory checkpoint for this session. No tracked files exist yet.",
        );
    }

    let mut out = format!(
        "Saved a FileHistory checkpoint for {} tracked file(s):\n\n",
        summary.tracked_files
    );
    for path in summary.current_paths {
        out.push_str(&format!("  {}\n", path.display()));
    }
    ToolResult::success(out)
}

fn action_revisions(session_id: &str, path: &Path) -> ToolResult {
    let revisions = match tracked_revisions(session_id, path) {
        Ok(revisions) => revisions,
        Err(err) => return err,
    };
    if revisions.is_empty() {
        return ToolResult::success(format!(
            "File {} is tracked in XFileStorage but has no retained revisions.",
            path.display()
        ));
    }

    let current_revision = revisions.last().map(|revision| revision.number);
    let mut out = format!("Revisions for {}:\n\n", path.display());
    for revision in revisions {
        let rendered = render_file(&revision.file);
        let current = if Some(revision.number) == current_revision {
            ", current"
        } else {
            ""
        };
        let metadata = revision
            .metadata
            .as_ref()
            .map(format_revision_metadata)
            .unwrap_or_default();
        out.push_str(&format!(
            "  rev {}{} — state: {}, lines: {}, bytes: {}{}\n",
            revision.number,
            current,
            file_state(&revision.file),
            revision.file.content.len(),
            rendered.len(),
            metadata,
        ));
    }

    ToolResult::success(out)
}

fn action_get_revision(session_id: &str, path: &Path, revision: usize) -> ToolResult {
    match get_revision(session_id, path, revision) {
        Some(found) if found.file.exists => ToolResult::success(render_file(&found.file)),
        Some(_) => ToolResult::success(format!(
            "Revision {} for {} represents an absent file.",
            revision,
            path.display()
        )),
        None => ToolResult::error(format!(
            "Revision {} not found for {}. Use action `revisions` to see available revisions.",
            revision,
            path.display()
        )),
    }
}

fn action_diff(
    session_id: &str,
    path: &Path,
    from_revision: Option<usize>,
    to_revision: Option<usize>,
) -> ToolResult {
    let revisions = match tracked_revisions(session_id, path) {
        Ok(revisions) => revisions,
        Err(err) => return err,
    };
    if revisions.is_empty() {
        return ToolResult::error(format!(
            "No retained XFileStorage revisions for {}.",
            path.display()
        ));
    }

    let current = revisions.last().expect("checked non-empty revisions");
    let previous = if revisions.len() >= 2 {
        Some(&revisions[revisions.len() - 2])
    } else {
        None
    };
    let earliest = revisions.first().expect("checked non-empty revisions");

    let from = match from_revision {
        Some(number) => match revisions.iter().find(|revision| revision.number == number) {
            Some(revision) => revision,
            None => {
                return ToolResult::error(format!(
                    "Revision {} not found for {}.",
                    number,
                    path.display()
                ))
            }
        },
        None => match to_revision {
            Some(_) => earliest,
            None => match previous {
                Some(previous) => previous,
                None => {
                    return ToolResult::error(format!(
                        "At least two retained revisions are required to diff {} without explicit revisions.",
                        path.display()
                    ))
                }
            },
        },
    };

    let to = match to_revision {
        Some(number) => match revisions.iter().find(|revision| revision.number == number) {
            Some(revision) => revision,
            None => {
                return ToolResult::error(format!(
                    "Revision {} not found for {}.",
                    number,
                    path.display()
                ))
            }
        },
        None => current,
    };

    let diff = diff_files(
        &from.file,
        &to.file,
        &format!("rev {}", from.number),
        &format!("rev {}", to.number),
    );

    if files_differ(&from.file, &to.file) {
        ToolResult::success(diff)
    } else {
        ToolResult::success("No differences between the selected revisions.")
    }
}

fn action_checkpoint_diff(session_id: &str) -> ToolResult {
    let summary = match diff_against_checkpoint(session_id) {
        Ok(summary) => summary,
        Err(err) => return ToolResult::error(err),
    };
    if summary.entries.is_empty() {
        let baseline = if summary.used_explicit_checkpoint {
            "saved checkpoint"
        } else {
            "implicit session-start baseline"
        };
        return ToolResult::success(format!(
            "No differences between the current tracked session state and the {}.",
            baseline
        ));
    }

    let baseline = if summary.used_explicit_checkpoint {
        "saved checkpoint"
    } else {
        "implicit session-start baseline"
    };
    let mut out = format!(
        "Combined diff between the current tracked session state and the {}:\n\n",
        baseline
    );
    for (idx, entry) in summary.entries.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if entry.baseline_file.path == entry.current_file.path {
            out.push_str(&format!("File: {}\n", entry.current_file.path.display()));
        } else {
            out.push_str(&format!(
                "File: {} -> {}\n",
                entry.baseline_file.path.display(),
                entry.current_file.path.display()
            ));
        }
        out.push_str(&diff_files(
            &entry.baseline_file,
            &entry.current_file,
            &format!("rev {}", entry.baseline_revision),
            &format!("rev {} (current)", entry.current_revision),
        ));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    ToolResult::success(out.trim_end().to_string())
}

async fn action_revert(session_id: &str, path: &Path, revision: usize) -> ToolResult {
    let revisions = match tracked_revisions(session_id, path) {
        Ok(revisions) => revisions,
        Err(err) => return err,
    };
    let current = match revisions.last() {
        Some(current) => current,
        None => {
            return ToolResult::error(format!(
                "No retained XFileStorage revisions for {}.",
                path.display()
            ))
        }
    };
    let target = match revisions
        .iter()
        .find(|candidate| candidate.number == revision)
    {
        Some(target) => target,
        None => {
            return ToolResult::error(format!(
                "Revision {} not found for {}. Use action `revisions` to see available revisions.",
                revision,
                path.display()
            ))
        }
    };

    if let Err(err) = apply_file_transition_to_disk(&current.file, &target.file).await {
        return ToolResult::error(err);
    }

    let head = match restore_revision(session_id, path, revision) {
        Ok(head) => head,
        Err(err) => return ToolResult::error(err),
    };
    if let Err(err) = record_disk_state(session_id, &head.file.path) {
        return ToolResult::error(err);
    }
    let diff = diff_files(
        &current.file,
        &head.file,
        &format!("rev {}", current.number),
        &format!(
            "restored from rev {} at {}",
            revision,
            head.file.path.display()
        ),
    );

    ToolResult::success(format!(
        "Restored {} to revision {} through XFileStorage at {}. A new head revision was created.\n{}",
        path.display(),
        revision,
        head.file.path.display(),
        diff.trim_end()
    ))
}

async fn action_rollback(session_id: &str) -> ToolResult {
    let summary = match rollback_to_checkpoint(session_id).await {
        Ok(summary) => summary,
        Err(err) => return ToolResult::error(err),
    };

    let checkpoint_kind = if summary.used_explicit_checkpoint {
        "saved checkpoint"
    } else {
        "implicit session-start baseline"
    };
    let mut out = format!(
        "Rolled back FileHistory state to the {}. Changed: {}, removed: {}, unchanged: {}.",
        checkpoint_kind, summary.changed_files, summary.removed_files, summary.unchanged_files
    );
    if !summary.affected_paths.is_empty() {
        out.push_str("\n\nAffected paths:\n");
        for path in summary.affected_paths {
            out.push_str(&format!("  {}\n", path.display()));
        }
    }
    ToolResult::success(out)
}

fn format_revision_metadata(metadata: &XFileRevisionMetadata) -> String {
    let mut details = Vec::new();

    if let Some(operation) = metadata.operation.as_deref() {
        details.push(format!("op: {}", operation));
    }
    if let Some(moved) = &metadata.moved {
        details.push(format!(
            "moved: {} -> {}",
            moved.source_path.display(),
            moved.destination_path.display()
        ));
    }
    if let Some(copied) = &metadata.copied {
        details.push(format!(
            "copied: {} -> {}",
            copied.source_path.display(),
            copied.destination_path.display()
        ));
    }

    if details.is_empty() {
        String::new()
    } else {
        format!(", {}", details.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tool::FileTool;
    use crate::file_xwrite::XWriteTool;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{
        clear_session_xfile_storage, list_revisions, store_loaded_if_missing, try_get_head,
    };
    use serde_json::json;
    use std::sync::Arc;
    use uuid::Uuid;

    fn make_ctx(root: &Path) -> ToolContext {
        ToolContext {
            working_dir: root.to_path_buf(),
            session_id: format!("history-test-{}", Uuid::new_v4()),
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

    async fn write_revisions(ctx: &ToolContext, file_path: &str, revisions: &[&str]) {
        for content in revisions {
            xwrite(ctx, file_path, content).await;
        }
    }

    fn json_input(action: &str, extra: serde_json::Value) -> Value {
        let mut map = extra.as_object().cloned().unwrap_or_default();
        map.insert("action".to_string(), serde_json::json!(action));
        Value::Object(map)
    }

    #[tokio::test]
    async fn list_reports_empty_xstorage() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);

        let tool = FileHistoryTool;
        let result = tool.execute(json_input("list", json!({})), &ctx).await;

        assert!(!result.is_error);
        assert!(result
            .content
            .contains("No XFileStorage-backed files tracked"));
    }

    #[tokio::test]
    async fn list_and_revisions_use_xstorage_only() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        xwrite(&ctx, &file_path, "alpha\n").await;
        xwrite(&ctx, &file_path, "beta\n").await;

        let tool = FileHistoryTool;
        let list = tool.execute(json_input("list", json!({})), &ctx).await;
        assert!(!list.is_error, "{}", list.content);
        assert!(list.content.contains("sample.txt"));
        assert!(list.content.contains("revisions: 3"));
        assert!(list.content.contains("state: present"));

        let revisions = tool
            .execute(
                json_input("revisions", json!({ "file_path": file_path })),
                &ctx,
            )
            .await;
        assert!(!revisions.is_error, "{}", revisions.content);
        assert!(revisions.content.contains("rev 1"));
        assert!(revisions.content.contains("state: absent"));
        assert!(revisions.content.contains("rev 3, current"));
    }

    #[tokio::test]
    async fn get_revision_returns_retained_content() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        xwrite(&ctx, &file_path, "first\n").await;
        xwrite(&ctx, &file_path, "second\n").await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "get_revision",
                    json!({
                        "file_path": file_path,
                        "revision": 2
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "first\n");
    }

    #[tokio::test]
    async fn get_revision_reports_absent_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        xwrite(&ctx, &file_path, "first\n").await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "get_revision",
                    json!({
                        "file_path": file_path,
                        "revision": 1
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("represents an absent file"));
    }

    #[tokio::test]
    async fn diff_defaults_to_previous_vs_current_head() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        xwrite(&ctx, &file_path, "first\n").await;
        xwrite(&ctx, &file_path, "second\n").await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(json_input("diff", json!({ "file_path": file_path })), &ctx)
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("--- rev 2"));
        assert!(result.content.contains("+++ rev 3"));
        assert!(result.content.contains("-first"));
        assert!(result.content.contains("+second"));
    }

    #[tokio::test]
    async fn diff_accepts_explicit_revision_range() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        write_revisions(&ctx, &file_path, &["first\n", "second\n", "third\n"]).await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "diff",
                    json!({
                        "file_path": file_path,
                        "from_revision": 2,
                        "to_revision": 4
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("--- rev 2"));
        assert!(result.content.contains("+++ rev 4"));
        assert!(result.content.contains("-first"));
        assert!(result.content.contains("+third"));
    }

    #[tokio::test]
    async fn diff_to_revision_defaults_from_earliest_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        write_revisions(&ctx, &file_path, &["first\n", "second\n", "third\n"]).await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "diff",
                    json!({
                        "file_path": file_path,
                        "to_revision": 2
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("--- rev 1"));
        assert!(result.content.contains("+++ rev 2"));
        assert!(result.content.contains("(absent)"));
        assert!(result.content.contains("(present)"));
        assert!(result.content.contains("+first"));
    }

    #[tokio::test]
    async fn diff_same_revision_reports_no_differences() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        write_revisions(&ctx, &file_path, &["first\n", "second\n"]).await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "diff",
                    json!({
                        "file_path": file_path,
                        "from_revision": 2,
                        "to_revision": 2
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            result.content,
            "No differences between the selected revisions."
        );
    }

    #[tokio::test]
    async fn diff_without_explicit_bounds_requires_two_revisions() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        let path = PathBuf::from(&file_path);
        tokio::fs::write(&path, "only\n").await.unwrap();
        store_loaded_if_missing(&ctx.session_id, &path, "only\n");

        let tool = FileHistoryTool;
        let result = tool
            .execute(json_input("diff", json!({ "file_path": file_path })), &ctx)
            .await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("At least two retained revisions are required"));
    }

    #[tokio::test]
    async fn diff_unknown_revision_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        write_revisions(&ctx, &file_path, &["first\n", "second\n"]).await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "diff",
                    json!({
                        "file_path": file_path,
                        "from_revision": 99
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Revision 99 not found"));
    }

    #[tokio::test]
    async fn diff_without_file_path_reports_combined_diff_since_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let edited = tmp.path().join("edited.txt");
        let created = tmp.path().join("created.txt");

        xwrite(&ctx, &edited.display().to_string(), "alpha\n").await;

        let tool = FileHistoryTool;
        let checkpoint = tool
            .execute(json_input("checkpoint", json!({})), &ctx)
            .await;
        assert!(!checkpoint.is_error, "{}", checkpoint.content);

        xwrite(&ctx, &edited.display().to_string(), "beta\n").await;
        xwrite(&ctx, &created.display().to_string(), "new\n").await;

        let result = tool.execute(json_input("diff", json!({})), &ctx).await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("saved checkpoint"));
        assert!(result
            .content
            .contains(&format!("File: {}", edited.display())));
        assert!(result
            .content
            .contains(&format!("File: {}", created.display())));
        assert!(result.content.contains("-alpha"));
        assert!(result.content.contains("+beta"));
        assert!(result.content.contains("+new"));
        assert!(result.content.contains("(absent)"));
    }

    #[tokio::test]
    async fn diff_without_file_path_uses_implicit_session_start_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let existing = tmp.path().join("existing.txt");

        tokio::fs::write(&existing, "before\n").await.unwrap();
        xwrite(&ctx, &existing.display().to_string(), "after\n").await;

        let tool = FileHistoryTool;
        let result = tool.execute(json_input("diff", json!({})), &ctx).await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("implicit session-start baseline"));
        assert!(result
            .content
            .contains(&format!("File: {}", existing.display())));
        assert!(result.content.contains("-before"));
        assert!(result.content.contains("+after"));
    }

    #[tokio::test]
    async fn checkpoint_diff_rejects_revision_bounds_without_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "diff",
                    json!({
                        "from_revision": 1
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("`from_revision` and `to_revision` require `file_path`"));
    }

    #[tokio::test]
    async fn revert_clones_requested_revision_into_new_head() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path_str = file_path.display().to_string();

        xwrite(&ctx, &file_path_str, "first\n").await;
        xwrite(&ctx, &file_path_str, "second\n").await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "revert",
                    json!({
                        "file_path": file_path_str,
                        "revision": 2
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            tokio::fs::read_to_string(&file_path).await.unwrap(),
            "first\n"
        );

        let revisions = list_revisions(&ctx.session_id, &file_path).unwrap();
        assert_eq!(revisions.len(), 4);
        assert_eq!(revisions.last().unwrap().number, 4);
        assert_eq!(render_file(&revisions.last().unwrap().file), "first\n");
    }

    #[tokio::test]
    async fn restore_alias_clones_requested_revision_into_new_head() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path_str = file_path.display().to_string();

        write_revisions(&ctx, &file_path_str, &["first\n", "second\n"]).await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "restore",
                    json!({
                        "file_path": file_path_str,
                        "revision": 2
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            tokio::fs::read_to_string(&file_path).await.unwrap(),
            "first\n"
        );
        assert!(result.content.contains("Restored"));

        let revisions = list_revisions(&ctx.session_id, &file_path).unwrap();
        assert_eq!(revisions.len(), 4);
        assert_eq!(revisions.last().unwrap().number, 4);
        assert_eq!(render_file(&revisions.last().unwrap().file), "first\n");
    }

    #[tokio::test]
    async fn revert_to_absent_revision_deletes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path_str = file_path.display().to_string();

        xwrite(&ctx, &file_path_str, "first\n").await;
        xwrite(&ctx, &file_path_str, "second\n").await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "revert",
                    json!({
                        "file_path": file_path_str,
                        "revision": 1
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(!file_path.exists());

        let revisions = list_revisions(&ctx.session_id, &file_path).unwrap();
        assert_eq!(revisions.last().unwrap().number, 4);
        assert!(!revisions.last().unwrap().file.exists);
    }

    #[tokio::test]
    async fn checkpoint_and_rollback_restore_session_state() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);

        let source = tmp.path().join("source.txt");
        let deleted = tmp.path().join("deleted.txt");
        let copy = tmp.path().join("copy.txt");
        let moved = tmp.path().join("moved/source.txt");

        xwrite(&ctx, &source.display().to_string(), "alpha\n").await;
        xwrite(&ctx, &deleted.display().to_string(), "delete-me\n").await;

        let tool = FileHistoryTool;
        let checkpoint = tool
            .execute(json_input("checkpoint", json!({})), &ctx)
            .await;
        assert!(!checkpoint.is_error, "{}", checkpoint.content);
        assert!(checkpoint
            .content
            .contains("Saved a FileHistory checkpoint"));

        let file_tool = FileTool;
        for input in [
            json!({
                "action": "copy",
                "source_path": source.display().to_string(),
                "destination_path": copy.display().to_string()
            }),
            json!({
                "action": "move",
                "source_path": source.display().to_string(),
                "destination_path": moved.display().to_string()
            }),
            json!({
                "action": "delete",
                "file_path": deleted.display().to_string()
            }),
        ] {
            let result = file_tool.execute(input, &ctx).await;
            assert!(!result.is_error, "{}", result.content);
        }

        let rollback = tool.execute(json_input("rollback", json!({})), &ctx).await;
        assert!(!rollback.is_error, "{}", rollback.content);
        assert!(rollback.content.contains("saved checkpoint"));
        assert!(rollback.content.contains("removed: 1"));

        assert_eq!(tokio::fs::read_to_string(&source).await.unwrap(), "alpha\n");
        assert!(!copy.exists());
        assert!(!moved.exists());
        assert_eq!(
            tokio::fs::read_to_string(&deleted).await.unwrap(),
            "delete-me\n"
        );

        assert!(try_get_head(&ctx.session_id, &copy).is_none());
        assert!(try_get_head(&ctx.session_id, &moved).is_none());
        assert_eq!(list_revisions(&ctx.session_id, &source).unwrap().len(), 2);
        assert_eq!(list_revisions(&ctx.session_id, &deleted).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);

        let tool = FileHistoryTool;
        let result = tool.execute(json_input("nonsense", json!({})), &ctx).await;

        assert!(result.is_error);
        assert!(result.content.contains("Unknown action: `nonsense`"));
    }

    #[tokio::test]
    async fn missing_file_path_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);

        let tool = FileHistoryTool;
        let result = tool.execute(json_input("revisions", json!({})), &ctx).await;

        assert!(result.is_error);
        assert_eq!(result.content, "`file_path` is required for this action");
    }

    #[tokio::test]
    async fn missing_revision_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("sample.txt");
        let file_path = file_path.display().to_string();

        xwrite(&ctx, &file_path, "first\n").await;

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "get_revision",
                    json!({
                        "file_path": file_path
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, "`revision` is required for this action");
    }

    #[tokio::test]
    async fn untracked_file_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        clear_session_xfile_storage(&ctx.session_id);
        let file_path = tmp.path().join("missing.txt");

        let tool = FileHistoryTool;
        let result = tool
            .execute(
                json_input(
                    "revisions",
                    json!({
                        "file_path": file_path.display().to_string()
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("not tracked in XFileStorage"));
    }
}
