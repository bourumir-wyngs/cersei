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
        let previous_snapshot =
            LAST_EDIT_SNAPSHOT_REGISTRY.insert(ctx.session_id.clone(), snapshot);

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
    let candidate = Path::new(input);
    if candidate.is_absolute() {
        return Err(ToolResult::error(
            "Absolute paths are not allowed; use a workspace-relative path.",
        ));
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolResult::error(
            "Path traversal with `..` is not allowed.",
        ));
    }

    let path = ctx.working_dir.join(candidate);
    if require_exists && !path.exists() {
        return Err(ToolResult::error(format!(
            "File not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

fn restore_previous_snapshot(
    session_id: &str,
    previous: Option<LastEditSnapshot>,
) -> Option<LastEditSnapshot> {
    match previous {
        Some(snapshot) => LAST_EDIT_SNAPSHOT_REGISTRY.insert(session_id.to_string(), snapshot),
        None => LAST_EDIT_SNAPSHOT_REGISTRY
            .remove(session_id)
            .map(|(_, snapshot)| snapshot),
    }
}
