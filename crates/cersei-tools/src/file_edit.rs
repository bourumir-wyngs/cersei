//! sed-backed file editing with one-step session-local revert.

use super::*;
use crate::file_history::{unified_diff, FileHistory};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
struct LastEditSnapshot {
    file_path: PathBuf,
    content: Vec<u8>,
}

static LAST_EDIT_SNAPSHOT_REGISTRY: Lazy<dashmap::DashMap<String, LastEditSnapshot>> =
    Lazy::new(dashmap::DashMap::new);

pub struct SedTool;
pub struct RevertTool;

/// Legacy type alias kept so downstream imports still compile.
pub type FileEditTool = SedTool;

#[async_trait]
impl Tool for SedTool {
    fn name(&self) -> &str {
        "Sed"
    }

    fn description(&self) -> &str {
        "Apply a sed script to a file using sed-rs (Rust regex / ERE). NOTE: unlike standard GNU sed, characters like `{`, `}`, `(`, `)`, `+`, and `?` are special by default and must be escaped (e.g., `\\{`) to match literals in code. The file is checkpointed \
         before the write, the result is written back to disk, and the tool returns a unified \
         diff plus the reminder \"use 'revert' command if wrong\". Use `revert` to undo the \
         most recent successful sed edit in this session."
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
                "script": {
                    "type": "string",
                    "description": "Sed script using Extended Regular Expressions (ERE), e.g. `s/foo/bar/g`. Escape {, }, (, ), +, ? to match as literals!"
                },
                "quiet": {
                    "type": "boolean",
                    "description": "Suppress automatic printing of the pattern space (`-n` behavior)",
                    "default": false
                },
                "null_data": {
                    "type": "boolean",
                    "description": "Use NUL as the record delimiter (`-z` behavior)",
                    "default": false
                }
            },
            "required": ["file_path", "script"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            file_path: String,
            script: String,
            #[serde(default)]
            quiet: bool,
            #[serde(default)]
            null_data: bool,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = match resolve_existing_workspace_path(ctx, &input.file_path) {
            Ok(path) => path,
            Err(err) => return err,
        };

        let original_bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolResult::error(format!("Failed to read file: {}", e)),
        };
        let original_text = String::from_utf8_lossy(&original_bytes).into_owned();

        let mut sed = match sed_rs::Sed::new(&input.script) {
            Ok(sed) => sed,
            Err(e) => return ToolResult::error(format!("Invalid sed script: {}\n\nNOTE: Unlike standard GNU sed, this tool uses Rust regex (Extended Regular Expressions). Characters like `{{`, `}}`, `(`, `)`, `+`, and `?` are special by default and must be escaped (e.g., `\\{{`) to match literals in code. To use capture groups, do NOT escape the parentheses: use `(...)` instead of `\\(...\\)`.", e)),
        };
        sed.quiet(input.quiet).null_data(input.null_data);

        let updated_text = match sed.eval_bytes(&original_bytes) {
            Ok(output) => output,
            Err(e) => return ToolResult::error(format!("Failed to run sed script: {}", e)),
        };

        if updated_text.as_bytes() == original_bytes.as_slice() {
            return ToolResult::success(format!(
                "sed script produced no changes in {}.",
                path.display()
            ));
        }

        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.snapshot_before_write(&path, &original_text, "edit");
        }

        let snapshot = LastEditSnapshot {
            file_path: path.clone(),
            content: original_bytes.clone(),
        };
        let previous_snapshot = LAST_EDIT_SNAPSHOT_REGISTRY.insert(ctx.session_id.clone(), snapshot);

        if let Err(e) = write_bytes(&path, updated_text.as_bytes()).await {
            restore_previous_snapshot(&ctx.session_id, previous_snapshot);
            return ToolResult::error(format!("Failed to write file: {}", e));
        }

        let diff = unified_diff(
            &original_text,
            &updated_text,
            &format!("{} (before)", path.display()),
            &format!("{} (after)", path.display()),
        );

        ToolResult::success(format!(
            "Applied sed script to {}.\n{}\n\nuse 'revert' command if wrong",
            path.display(),
            diff.trim_end()
        ))
    }
}

#[async_trait]
impl Tool for RevertTool {
    fn name(&self) -> &str {
        "Revert"
    }

    fn description(&self) -> &str {
        "Restore the previous checkpoint from the most recent successful `sed` or `vicut` edit in this \
         session. Only one checkpoint is retained."
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
                    "description": "Optional safety check. If provided, it must be a workspace-relative path matching the file from the most recent sed edit."
                }
            }
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

        let snapshot = match LAST_EDIT_SNAPSHOT_REGISTRY.get(&ctx.session_id) {
            Some(entry) => entry.clone(),
            None => {
                return ToolResult::error(
                    "No sed snapshot is available to revert. Run `sed` first.",
                )
            }
        };

        if let Some(requested) = input.file_path.as_ref() {
            let requested_path = match resolve_workspace_path(ctx, requested, false) {
                Ok(path) => path,
                Err(err) => return err,
            };
            if requested_path != snapshot.file_path {
                return ToolResult::error(format!(
                    "The last sed snapshot is for {}, not {}.",
                    snapshot.file_path.display(),
                    requested
                ));
            }
        }

        let current_bytes = tokio::fs::read(&snapshot.file_path)
            .await
            .unwrap_or_default();
        let current_text = String::from_utf8_lossy(&current_bytes).into_owned();
        let restored_text = String::from_utf8_lossy(&snapshot.content).into_owned();

        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.snapshot_before_write(&snapshot.file_path, &current_text, "revert");
        }

        if let Err(e) = write_bytes(&snapshot.file_path, &snapshot.content).await {
            return ToolResult::error(format!("Failed to write file: {}", e));
        }

        LAST_EDIT_SNAPSHOT_REGISTRY.remove(&ctx.session_id);

        let diff = unified_diff(
            &current_text,
            &restored_text,
            &format!("{} (current)", snapshot.file_path.display()),
            &format!("{} (reverted)", snapshot.file_path.display()),
        );

        ToolResult::success(format!(
            "Reverted {} to the previous sed snapshot.\n{}",
            snapshot.file_path.display(),
            diff.trim_end()
        ))
    }
}

async fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes).await
}

fn resolve_existing_workspace_path(
    ctx: &ToolContext,
    input: &str,
) -> std::result::Result<PathBuf, ToolResult> {
    resolve_workspace_path(ctx, input, true)
}

fn resolve_workspace_path(
    ctx: &ToolContext,
    input: &str,
    require_exists: bool,
) -> std::result::Result<PathBuf, ToolResult> {
    let root = ctx.working_dir.canonicalize().map_err(|e| {
        ToolResult::error(format!(
            "Cannot resolve workspace root '{}': {}",
            ctx.working_dir.display(),
            e
        ))
    })?;

    let relative = normalize_relative_workspace_path(input)?;
    let candidate = root.join(&relative);

    if require_exists {
        let canonical = candidate
            .canonicalize()
            .map_err(|e| ToolResult::error(format!("Cannot access file '{}': {}", input, e)))?;
        if !canonical.starts_with(&root) {
            return Err(ToolResult::error(format!(
                "Path '{}' resolves outside the current workspace root.",
                input
            )));
        }
        Ok(canonical)
    } else {
        if let Ok(canonical) = candidate.canonicalize() {
            if !canonical.starts_with(&root) {
                return Err(ToolResult::error(format!(
                    "Path '{}' resolves outside the current workspace root.",
                    input
                )));
            }
            Ok(canonical)
        } else {
            Ok(candidate)
        }
    }
}

fn normalize_relative_workspace_path(input: &str) -> std::result::Result<PathBuf, ToolResult> {
    let raw = Path::new(input);
    if raw.is_absolute() {
        return Err(ToolResult::error(
            "file_path must be relative to the current workspace root; absolute paths are not allowed.",
        ));
    }

    let mut normalized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                return Err(ToolResult::error(
                    "file_path must stay within the current workspace root; `..` is not allowed.",
                ))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolResult::error(
                    "file_path must be relative to the current workspace root.",
                ))
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(ToolResult::error(
            "file_path must point to a file inside the current workspace root.",
        ));
    }

    Ok(normalized)
}

fn restore_previous_snapshot(session_id: &str, previous: Option<LastEditSnapshot>) {
    if let Some(snapshot) = previous {
        LAST_EDIT_SNAPSHOT_REGISTRY.insert(session_id.to_string(), snapshot);
    } else {
        LAST_EDIT_SNAPSHOT_REGISTRY.remove(session_id);
    }
}

#[cfg(test)]
fn clear_last_snapshot(session_id: &str) {
    LAST_EDIT_SNAPSHOT_REGISTRY.remove(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_history::FileHistory;
    use std::sync::Arc;
    use tempfile::TempDir;
    use uuid::Uuid;

    struct TestWorkspace {
        dir: TempDir,
        ctx: ToolContext,
    }

    impl TestWorkspace {
        fn new(session_id: String) -> Self {
            let dir = TempDir::new().unwrap();
            let ctx = ToolContext {
                session_id,
                working_dir: dir.path().to_path_buf(),
                ..ToolContext::default()
            };
            Self { dir, ctx }
        }

        fn with_history(session_id: String) -> (Self, Arc<FileHistory>) {
            let ws = Self::new(session_id);
            ws.ctx.extensions.insert(FileHistory::new());
            let history = ws.ctx.extensions.get::<FileHistory>().unwrap();
            (ws, history)
        }

        fn write_file(&self, rel: &str, content: &[u8]) -> PathBuf {
            let path = self.dir.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, content).unwrap();
            path
        }

        fn read_string(&self, rel: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(rel)).unwrap()
        }

        fn read_bytes(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.dir.path().join(rel)).unwrap()
        }
    }

    fn unique_session(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4())
    }

    #[tokio::test]
    async fn sed_edit_returns_diff_and_revert_restores_previous_content() {
        let session_id = unique_session("sed-test");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id.clone());

        ws.write_file("sample.txt", b"hello world\n");

        let tool = SedTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/world/rust/"
                }),
                &ws.ctx,
            )
            .await;

        assert!(!result.is_error, "sed failed: {}", result.content);
        assert!(result.content.contains("@@"));
        assert!(result.content.contains("-hello world"));
        assert!(result.content.contains("+hello rust"));
        assert!(result.content.contains("use 'revert' command if wrong"));
        assert_eq!(ws.read_string("sample.txt"), "hello rust\n");

        let revert = RevertTool;
        let reverted = revert.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(!reverted.is_error, "revert failed: {}", reverted.content);
        assert!(reverted.content.contains("Reverted"));
        assert_eq!(ws.read_string("sample.txt"), "hello world\n");

        let second_revert = revert.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(second_revert.is_error);
        assert!(second_revert.content.contains("No sed snapshot"));
    }

    #[tokio::test]
    async fn sed_reports_when_script_changes_nothing() {
        let session_id = unique_session("sed-noop");
        clear_last_snapshot(&session_id);
        let (ws, history) = TestWorkspace::with_history(session_id.clone());

        let file = ws.write_file("sample.txt", b"hello world\n");

        let tool = SedTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/xyz/abc/"
                }),
                &ws.ctx,
            )
            .await;

        assert!(!result.is_error, "sed failed: {}", result.content);
        assert!(result.content.contains("produced no changes"));
        assert_eq!(history.revision_count(&file), 0);

        let revert = RevertTool;
        let reverted = revert.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(reverted.is_error);
        assert!(reverted.content.contains("No sed snapshot"));
    }

    #[tokio::test]
    async fn sed_rejects_invalid_script() {
        let session_id = unique_session("sed-invalid");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id);
        ws.write_file("sample.txt", b"hello world\n");

        let tool = SedTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/[invalid/x/"
                }),
                &ws.ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Invalid sed script"));
    }

    #[tokio::test]
    async fn sed_rejects_absolute_paths() {
        let session_id = unique_session("sed-absolute");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id);
        let path = ws.write_file("sample.txt", b"hello world\n");

        let result = SedTool
            .execute(
                serde_json::json!({
                    "file_path": path.to_string_lossy().into_owned(),
                    "script": "s/world/rust/"
                }),
                &ws.ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("absolute paths are not allowed"));
    }

    #[tokio::test]
    async fn sed_rejects_parent_dir_traversal() {
        let session_id = unique_session("sed-traversal");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id);
        let outside_dir = TempDir::new().unwrap();
        let outside = outside_dir.path().join("secret.txt");
        std::fs::write(&outside, "secret\n").unwrap();

        let result = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "../secret.txt",
                    "script": "s/secret/public/"
                }),
                &ws.ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("`..` is not allowed"));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "secret\n");
    }

    #[tokio::test]
    async fn revert_rejects_mismatched_file_path() {
        let session_id = unique_session("sed-mismatch");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id);
        ws.write_file("sample.txt", b"hello world\n");

        SedTool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/world/rust/"
                }),
                &ws.ctx,
            )
            .await;

        let result = RevertTool
            .execute(serde_json::json!({"file_path": "other.txt"}), &ws.ctx)
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("not other.txt"));
        assert_eq!(ws.read_string("sample.txt"), "hello rust\n");
    }

    #[tokio::test]
    async fn revert_uses_only_the_latest_snapshot_in_a_session() {
        let session_id = unique_session("sed-latest");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id);
        ws.write_file("sample.txt", b"one two\n");

        let first = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/one/ONE/"
                }),
                &ws.ctx,
            )
            .await;
        assert!(!first.is_error, "first sed failed: {}", first.content);
        assert_eq!(ws.read_string("sample.txt"), "ONE two\n");

        let second = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/two/TWO/"
                }),
                &ws.ctx,
            )
            .await;
        assert!(!second.is_error, "second sed failed: {}", second.content);
        assert_eq!(ws.read_string("sample.txt"), "ONE TWO\n");

        let reverted = RevertTool.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(!reverted.is_error, "revert failed: {}", reverted.content);
        assert_eq!(ws.read_string("sample.txt"), "ONE two\n");

        let second_revert = RevertTool.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(second_revert.is_error);
        assert!(second_revert.content.contains("No sed snapshot"));
    }

    #[tokio::test]
    async fn snapshots_are_isolated_per_session() {
        let session_a = unique_session("sed-session-a");
        let session_b = unique_session("sed-session-b");
        clear_last_snapshot(&session_a);
        clear_last_snapshot(&session_b);

        let ws_a = TestWorkspace::new(session_a);
        let ws_b = TestWorkspace::new(session_b);
        ws_a.write_file("a.txt", b"alpha\n");
        ws_b.write_file("b.txt", b"beta\n");

        let result_a = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "a.txt",
                    "script": "s/alpha/ALPHA/"
                }),
                &ws_a.ctx,
            )
            .await;
        let result_b = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "b.txt",
                    "script": "s/beta/BETA/"
                }),
                &ws_b.ctx,
            )
            .await;

        assert!(
            !result_a.is_error,
            "session A sed failed: {}",
            result_a.content
        );
        assert!(
            !result_b.is_error,
            "session B sed failed: {}",
            result_b.content
        );

        let revert_a = RevertTool.execute(serde_json::json!({}), &ws_a.ctx).await;
        assert!(
            !revert_a.is_error,
            "session A revert failed: {}",
            revert_a.content
        );
        assert_eq!(ws_a.read_string("a.txt"), "alpha\n");
        assert_eq!(ws_b.read_string("b.txt"), "BETA\n");

        let revert_b = RevertTool.execute(serde_json::json!({}), &ws_b.ctx).await;
        assert!(
            !revert_b.is_error,
            "session B revert failed: {}",
            revert_b.content
        );
        assert_eq!(ws_b.read_string("b.txt"), "beta\n");
    }

    #[tokio::test]
    async fn sed_and_revert_record_file_history_revisions() {
        let session_id = unique_session("sed-history");
        clear_last_snapshot(&session_id);
        let (ws, history) = TestWorkspace::with_history(session_id);
        let path_buf = ws
            .write_file("sample.txt", b"hello world\n")
            .canonicalize()
            .unwrap();

        let edit = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/world/rust/"
                }),
                &ws.ctx,
            )
            .await;
        assert!(!edit.is_error, "sed failed: {}", edit.content);

        let revert = RevertTool.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(!revert.is_error, "revert failed: {}", revert.content);

        let revisions = history.get_revisions(&path_buf).unwrap();
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0].operation, "edit");
        assert_eq!(revisions[1].operation, "revert");
        assert_eq!(
            history.get_revision_content(&path_buf, 1).unwrap(),
            "hello world\n"
        );
        assert_eq!(
            history.get_revision_content(&path_buf, 2).unwrap(),
            "hello rust\n"
        );
    }

    #[tokio::test]
    async fn sed_reports_missing_file_read_failures() {
        let session_id = unique_session("sed-missing");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id.clone());

        let result = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "missing.txt",
                    "script": "s/foo/bar/"
                }),
                &ws.ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Cannot access file"));

        let revert = RevertTool.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(revert.is_error);
        assert!(revert.content.contains("No sed snapshot"));
    }

    #[tokio::test]
    async fn sed_write_failure_does_not_leave_a_revert_snapshot() {
        let session_id = unique_session("sed-write-fail");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id.clone());
        let path = ws.write_file("sample.txt", b"hello world\n");

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions.clone()).unwrap();

        let result = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/world/rust/"
                }),
                &ws.ctx,
            )
            .await;

        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Failed to write file"));
        assert_eq!(ws.read_string("sample.txt"), "hello world\n");

        let revert = RevertTool.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(revert.is_error);
        assert!(revert.content.contains("No sed snapshot"));
    }

    #[tokio::test]
    async fn failed_revert_keeps_the_snapshot_available() {
        let session_id = unique_session("sed-revert-fail");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id.clone());
        let path = ws.write_file("sample.txt", b"hello world\n");

        let edit = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "s/world/rust/"
                }),
                &ws.ctx,
            )
            .await;
        assert!(!edit.is_error, "sed failed: {}", edit.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust\n");

        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        let failed_revert = RevertTool.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(failed_revert.is_error);
        assert!(failed_revert.content.contains("Failed to write file"));

        std::fs::remove_dir(&path).unwrap();

        let recovered_revert = RevertTool.execute(serde_json::json!({}), &ws.ctx).await;
        assert!(
            !recovered_revert.is_error,
            "recovered revert failed: {}",
            recovered_revert.content
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world\n");
    }

    #[tokio::test]
    async fn sed_supports_quiet_mode() {
        let session_id = unique_session("sed-quiet");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id);
        ws.write_file("sample.txt", b"a\nb\nc\n");

        let result = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "sample.txt",
                    "script": "2p",
                    "quiet": true
                }),
                &ws.ctx,
            )
            .await;

        assert!(!result.is_error, "sed failed: {}", result.content);
        assert_eq!(ws.read_string("sample.txt"), "b\n");
    }

    #[tokio::test]
    async fn sed_supports_null_delimited_data() {
        let session_id = unique_session("sed-null");
        clear_last_snapshot(&session_id);
        let ws = TestWorkspace::new(session_id);
        ws.write_file("null.bin", b"foo\0bar\0");

        let result = SedTool
            .execute(
                serde_json::json!({
                    "file_path": "null.bin",
                    "script": "s/bar/baz/",
                    "null_data": true
                }),
                &ws.ctx,
            )
            .await;

        assert!(!result.is_error, "sed failed: {}", result.content);
        assert_eq!(ws.read_bytes("null.bin"), b"foo\0baz\0");
    }
}
