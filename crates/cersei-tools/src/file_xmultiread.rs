//! MultiRead tool: session-scoped tagged multi-file reads backed by XFileStorage.

use super::*;
use crate::file_xread::XReadRequest;
use serde::Deserialize;
use serde_json::Value;

pub struct XMultiReadTool;

/// Public alias preserved for downstream imports.
pub type FileXMultiReadTool = XMultiReadTool;

#[derive(Debug, Clone, Deserialize)]
struct XMultiReadRequest {
    requests: Vec<XReadRequest>,
}

#[async_trait]
impl Tool for XMultiReadTool {
    fn name(&self) -> &str {
        "MultiRead"
    }

    fn description(&self) -> &str {
        "Read multiple files in one request by providing a list of Read inputs. The output is the joined Read outputs with file markers between them."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        let read_schema = crate::file_xread::XReadTool.input_schema();
        serde_json::json!({
            "type": "object",
            "properties": {
                "requests": {
                    "type": "array",
                    "description": "List of Read-style request objects. Each item accepts the same fields as Read.",
                    "items": read_schema,
                    "minItems": 1
                }
            },
            "required": ["requests"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: XMultiReadRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        if req.requests.is_empty() {
            return ToolResult::error("`requests` must contain at least one Read request.");
        }

        let read_tool = crate::file_xread::XReadTool;
        let mut outputs = Vec::with_capacity(req.requests.len());
        let mut metadata = Vec::with_capacity(req.requests.len());

        for request in req.requests {
            let file_path = request.file_path.clone();
            let result = match serde_json::to_value(&request) {
                Ok(input) => read_tool.execute(input, ctx).await,
                Err(err) => {
                    return ToolResult::error(format!(
                        "Failed to serialize Read request for {}: {}",
                        file_path, err
                    ));
                }
            };

            if result.is_error {
                return ToolResult::error(format!(
                    "Reading file {}\n{}",
                    file_path, result.content
                ))
                .with_metadata(result.metadata.unwrap_or(Value::Null));
            }

            outputs.push(format!("Reading file {}\n{}", file_path, result.content));
            metadata.push(serde_json::json!({
                "file_path": file_path,
                "result": result.metadata.unwrap_or(Value::Null),
            }));
        }

        ToolResult::success(outputs.join("\n\n")).with_metadata(serde_json::json!({
            "requests": metadata,
            "request_count": outputs.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{clear_session_xfile_storage, store_written_text};
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
    fn filesystem_toolset_includes_xmultiread() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "MultiRead"));
    }

    #[tokio::test]
    async fn xmultiread_reads_multiple_files_and_joins_output() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xmultiread-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path_a = tmp.path().join("a.txt");
        let head_a = store_written_text(&session_id, &path_a, "one\ntwo\n");
        tokio::fs::write(&path_a, &head_a.rendered_content)
            .await
            .unwrap();

        let path_b = tmp.path().join("b.txt");
        let head_b = store_written_text(&session_id, &path_b, "three\nfour\n");
        tokio::fs::write(&path_b, &head_b.rendered_content)
            .await
            .unwrap();

        let tool = XMultiReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "requests": [
                        { "file_path": path_a.display().to_string() },
                        { "file_path": path_b.display().to_string() }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result
            .content
            .contains(&format!("Reading file {}", path_a.display())));
        assert!(result
            .content
            .contains(&format!("Reading file {}", path_b.display())));
        assert!(result.content.contains("\tone"));
        assert!(result.content.contains("\tthree"));
        assert_eq!(result.metadata.as_ref().unwrap()["request_count"], 2);
    }
    #[tokio::test]
    async fn xmultiread_reports_spreadsheet_hint_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xmultiread-spreadsheet-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path = tmp.path().join("sheet.xlsx");
        tokio::fs::write(&path, b"placeholder").await.unwrap();

        let tool = XMultiReadTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "requests": [
                        { "file_path": path.display().to_string() }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            format!(
                "Reading file {}\nUse SpreadSheet tool to read this format",
                path.display()
            )
        );
    }
}
