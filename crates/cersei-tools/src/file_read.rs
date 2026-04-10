//! File read tool.

use super::*;
use crate::file_history::FileHistory;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read a file from the filesystem."
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
                    "description": "Path to the file. Absolute paths and workspace-relative paths are accepted."
                },
                "offset": { "type": "integer", "description": "Line number to start reading from" },
                "limit": { "type": "integer", "description": "Number of lines to read" }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            file_path: String,
            offset: Option<usize>,
            limit: Option<usize>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = resolve_path(ctx, &input.file_path);
        if !path.exists() {
            return ToolResult::error(format!("File not found: {}", path.display()));
        }

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                // Track the read in file history
                if let Some(history) = ctx.extensions.get::<FileHistory>() {
                    history.record_read(&path);
                }

                let lines: Vec<&str> = content.lines().collect();
                let offset = input.offset.unwrap_or(0);
                let limit = input.limit.unwrap_or(2000);

                let selected: Vec<String> = lines
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .enumerate()
                    .map(|(i, line)| format!("{:>6}\t{}", offset + i + 1, line))
                    .collect();

                ToolResult::success(selected.join("\n"))
            }
            Err(e) => ToolResult::error(format!("Failed to read file: {}", e)),
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

    resolved.canonicalize().unwrap_or(resolved)
}
