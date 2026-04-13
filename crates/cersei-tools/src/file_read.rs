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
                "offset": { "type": "integer", "description": "1-based line number to start reading from" },
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

        if matches!(input.offset, Some(0)) {
            return ToolResult::error("String indices are 1 based.".to_string());
        }

        let path = resolve_path(ctx, &input.file_path);
        if !path.exists() {
            return ToolResult::error(format!("File not found: {}", path.display()));
        }

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                if let Some(history) = ctx.extensions.get::<FileHistory>() {
                    history.record_read(&path);
                }

                let lines: Vec<&str> = content.lines().collect();
                let start_line = input.offset.unwrap_or(1);
                let skip = start_line - 1;
                let limit = input.limit.unwrap_or(2000);

                let selected: Vec<String> = lines
                    .iter()
                    .skip(skip)
                    .take(limit)
                    .enumerate()
                    .map(|(i, line)| format!("{:>6}\t{}", start_line + i, line))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::CostTracker;
    use serde_json::json;
    use std::sync::Arc;

    fn test_ctx(root: &Path) -> ToolContext {
        ToolContext {
            working_dir: root.to_path_buf(),
            session_id: "file-read-test".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    #[tokio::test]
    async fn read_offset_is_one_based() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "line1\nline2\nline3\n")
            .await
            .unwrap();
        let ctx = test_ctx(tmp.path());
        let tool = FileReadTool;

        let result = tool
            .execute(
                json!({ "file_path": "sample.txt", "offset": 1, "limit": 2 }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "     1\tline1\n     2\tline2");
    }

    #[tokio::test]
    async fn read_offset_two_starts_at_line_two() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "line1\nline2\nline3\n")
            .await
            .unwrap();
        let ctx = test_ctx(tmp.path());
        let tool = FileReadTool;

        let result = tool
            .execute(
                json!({ "file_path": "sample.txt", "offset": 2, "limit": 2 }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "     2\tline2\n     3\tline3");
    }

    #[tokio::test]
    async fn read_offset_zero_returns_one_based_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "line1\nline2\nline3\n")
            .await
            .unwrap();
        let ctx = test_ctx(tmp.path());
        let tool = FileReadTool;

        let result = tool
            .execute(
                json!({ "file_path": "sample.txt", "offset": 0, "limit": 2 }),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, "String indices are 1 based.");
    }
}
