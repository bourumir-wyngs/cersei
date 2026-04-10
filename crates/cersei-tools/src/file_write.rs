//! File write tool.

use super::*;
use crate::file_history::FileHistory;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Write content to a file, creating it if it doesn't exist."
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
                    "description": "Path to the file. Absolute paths and workspace-relative paths are accepted."
                },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["file_path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            file_path: String,
            content: String,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = resolve_path(ctx, &input.file_path);

        let previous_content = tokio::fs::read_to_string(&path).await.ok();

        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::error(format!("Failed to create directories: {}", e));
            }
        }

        match tokio::fs::write(&path, &input.content).await {
            Ok(()) => {
                if let Some(history) = ctx.extensions.get::<FileHistory>() {
                    history.record_change(
                        &path,
                        previous_content.as_deref(),
                        &input.content,
                        "write",
                    );
                }
                let char_count = input.content.chars().count();
                ToolResult::success(format!("Wrote {char_count} chars to: {}", path.display()))
            }
            Err(e) => ToolResult::error(format!("Failed to write file: {}", e)),
        }
    }
}

fn resolve_path(ctx: &ToolContext, input: &str) -> PathBuf {
    let candidate = Path::new(input);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ctx.working_dir.join(candidate)
    };

    if let Ok(canonical) = resolved.canonicalize() {
        return canonical;
    }

    if let Some(parent) = resolved.parent() {
        if let Ok(parent_canonical) = parent.canonicalize() {
            if let Some(file_name) = resolved.file_name() {
                return parent_canonical.join(file_name);
            }
        }
    }

    resolved
}
