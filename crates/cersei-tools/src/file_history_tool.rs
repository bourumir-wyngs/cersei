//! FileHistory tool: exposes XFileStorage revision history to the AI.
//!
//! Actions: list, revisions, get_revision, diff, revert, restore.

use super::*;
use crate::file_history::unified_diff;
use crate::xfile_storage::{
    get_revision, list_revisions, list_tracked_files, render_file, resolve_xfile_path,
    restore_revision,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct FileHistoryTool;

#[async_trait]
impl Tool for FileHistoryTool {
    fn name(&self) -> &str {
        "FileHistory"
    }

    fn description(&self) -> &str {
        "Inspect session-scoped XFileStorage revision history for files loaded by Read, Write, Edit, or matching Grep results. History is limited to the retained XFileStorage revisions for this session. \
         Actions:\n\
         - `list` — list tracked XFileStorage files with current revision and line counts.\n\
         - `revisions` — list retained revisions for a specific file (requires `file_path`).\n\
         - `get_revision` — get the full rendered content of a stored revision (requires `file_path` + `revision`).\n\
         - `diff` — show a unified diff between retained revisions. Omitting `to_revision` diffs against the current XFileStorage head. \
           Omitting `from_revision` defaults to the earliest retained revision. Omitting both diffs the previous revision against the current head.\n\
         - `revert` — restore a file to a specific retained revision by cloning that revision into a new XFileStorage head and flushing it to disk (requires `file_path` + `revision`).\n\
         - `restore` — same as revert (alias)."
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
                    "enum": ["list", "revisions", "get_revision", "diff", "revert", "restore"],
                    "description": "The action to perform. `list` needs no extra fields. `revisions` and `diff` need `file_path`. `get_revision`, `revert`, and `restore` need both `file_path` and `revision`."
                },
                "file_path": {
                    "type": "string",
                    "description": "Path to the tracked file. Required for every action except `list`. Absolute paths and workspace-relative paths are accepted."
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
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

        match input.action.as_str() {
            "list" => action_list(&ctx.session_id),
            "revisions" => {
                let path = match require_path(&input.file_path, ctx) {
                    Ok(path) => path,
                    Err(err) => return err,
                };
                action_revisions(&ctx.session_id, &path)
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
                action_get_revision(&ctx.session_id, &path, revision)
            }
            "diff" => {
                let path = match require_path(&input.file_path, ctx) {
                    Ok(path) => path,
                    Err(err) => return err,
                };
                action_diff(&ctx.session_id, &path, input.from_revision, input.to_revision)
            }
            "revert" | "restore" => {
                let path = match require_path(&input.file_path, ctx) {
                    Ok(path) => path,
                    Err(err) => return err,
                };
                let revision = match require_revision(input.revision) {
                    Ok(revision) => revision,
                    Err(err) => return err,
                };
                action_revert(&ctx.session_id, &path, revision).await
            }
            other => ToolResult::error(format!(
                "Unknown action: `{}`. Valid actions: list, revisions, get_revision, diff, revert, restore",
                other
            )),
        }
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
            "  {} — revisions: {}, current: rev {}, lines: {}\n",
            file.path.display(),
            file.revision_count,
            file.current_revision,
            file.line_count,
        ));
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
        out.push_str(&format!(
            "  rev {}{} — lines: {}, bytes: {}\n",
            revision.number,
            current,
            revision.file.content.len(),
            rendered.len(),
        ));
    }

    ToolResult::success(out)
}

fn action_get_revision(session_id: &str, path: &Path, revision: usize) -> ToolResult {
    match get_revision(session_id, path, revision) {
        Some(revision) => ToolResult::success(render_file(&revision.file)),
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

    let diff = unified_diff(
        &render_file(&from.file),
        &render_file(&to.file),
        &format!("rev {}", from.number),
        &format!("rev {}", to.number),
    );

    if diff.contains("@@") {
        ToolResult::success(diff)
    } else {
        ToolResult::success("No differences between the selected revisions.")
    }
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

    let current_text = render_file(&current.file);
    let target_text = render_file(&target.file);
    if let Err(err) = tokio::fs::write(path, target_text.as_bytes()).await {
        return ToolResult::error(format!("Failed to write file: {}", err));
    }

    let head = match restore_revision(session_id, path, revision) {
        Ok(head) => head,
        Err(err) => return ToolResult::error(err),
    };
    let diff = unified_diff(
        &current_text,
        &head.rendered_content,
        &format!("rev {}", current.number),
        &format!("restored from rev {}", revision),
    );

    ToolResult::success(format!(
        "Restored {} to revision {} through XFileStorage. A new head revision was created.\n{}",
        path.display(),
        revision,
        diff.trim_end()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_xwrite::XWriteTool;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{clear_session_xfile_storage, list_revisions};
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
        assert!(list.content.contains("revisions: 2"));

        let revisions = tool
            .execute(
                json_input("revisions", json!({ "file_path": file_path })),
                &ctx,
            )
            .await;
        assert!(!revisions.is_error, "{}", revisions.content);
        assert!(revisions.content.contains("rev 1"));
        assert!(revisions.content.contains("rev 2, current"));
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
                        "revision": 1
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "first\n");
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
        assert!(result.content.contains("--- rev 1"));
        assert!(result.content.contains("+++ rev 2"));
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
                        "from_revision": 1,
                        "to_revision": 3
                    }),
                ),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("--- rev 1"));
        assert!(result.content.contains("+++ rev 3"));
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
        assert!(result.content.contains("-first"));
        assert!(result.content.contains("+second"));
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

        xwrite(&ctx, &file_path, "only\n").await;

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
                        "revision": 1
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
        assert_eq!(revisions.len(), 3);
        assert_eq!(revisions.last().unwrap().number, 3);
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
                        "revision": 1
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
        assert_eq!(revisions.len(), 3);
        assert_eq!(revisions.last().unwrap().number, 3);
        assert_eq!(render_file(&revisions.last().unwrap().file), "first\n");
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
