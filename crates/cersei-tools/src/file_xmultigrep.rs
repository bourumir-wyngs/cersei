//! MultiGrep tool: session-scoped tagged multi-file search backed by XFileStorage.

use super::*;
use crate::file_xgrep::XGrepRequest;
use serde::Deserialize;
use serde_json::Value;

pub struct XMultiGrepTool;

/// Public alias preserved for downstream imports.
pub type FileXMultiGrepTool = XMultiGrepTool;

#[derive(Debug, Clone, Deserialize)]
struct XMultiGrepRequest {
    requests: Vec<XGrepRequest>,
}

#[async_trait]
impl Tool for XMultiGrepTool {
    fn name(&self) -> &str {
        "MultiGrep"
    }

    fn description(&self) -> &str {
        "Perform multiple regex searches in a single request. Highly recommended for gathering context from several related parts of the codebase simultaneously, significantly reducing conversational turns and providing a unified view of search results."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        let grep_schema = crate::file_xgrep::XGrepTool.input_schema();
        serde_json::json!({
            "type": "object",
            "properties": {
                "requests": {
                    "type": "array",
                    "description": "List of Grep-style request objects. Each item accepts the same fields as Grep.",
                    "items": grep_schema,
                    "minItems": 1
                }
            },
            "required": ["requests"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: XMultiGrepRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        if req.requests.is_empty() {
            return ToolResult::error("`requests` must contain at least one Grep request.");
        }

        let grep_tool = crate::file_xgrep::XGrepTool;
        let mut outputs = Vec::with_capacity(req.requests.len());
        let mut metadata = Vec::with_capacity(req.requests.len());

        for mut request in req.requests {
            request.suppress_nudge = Some(true);
            let result = match serde_json::to_value(&request) {
                Ok(input) => grep_tool.execute(input, ctx).await,
                Err(err) => {
                    return ToolResult::error(format!(
                        "Failed to serialize Grep request for pattern '{}': {}",
                        request.pattern, err
                    ));
                }
            };

            if result.is_error {
                return ToolResult::error(format!(
                    "Grep failed for pattern '{}' in path '{}'\n{}",
                    request.pattern, request.path, result.content
                ))
                .with_metadata(result.metadata.unwrap_or(Value::Null));
            }

            let params_summary = format!(
                "Grep pattern='{}' path='{}' glob={:?} before={:?} after={:?}",
                request.pattern,
                request.path,
                request.glob.as_deref().unwrap_or("None"),
                request.before.unwrap_or(0),
                request.after.unwrap_or(0)
            );

            outputs.push(format!("### {}\n{}", params_summary, result.content));
            metadata.push(serde_json::json!({
                "request": request,
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

    #[tokio::test]
    async fn xmultigrep_performs_multiple_searches() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xmultigrep-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let path_a = tmp.path().join("a.txt");
        let head_a = store_written_text(&session_id, &path_a, "apple\nbanana\n");
        tokio::fs::write(&path_a, &head_a.rendered_content)
            .await
            .unwrap();

        let path_b = tmp.path().join("b.txt");
        let head_b = store_written_text(&session_id, &path_b, "cherry\ndate\n");
        tokio::fs::write(&path_b, &head_b.rendered_content)
            .await
            .unwrap();

        let tool = XMultiGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "requests": [
                        { "pattern": "apple", "path": tmp.path().display().to_string() },
                        { "pattern": "cherry", "path": tmp.path().display().to_string() }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("pattern='apple'"));
        assert!(result.content.contains("pattern='cherry'"));
        assert!(result.content.contains("a.txt"));
        assert!(result.content.contains("b.txt"));
        assert!(result.content.contains("apple"));
        assert!(result.content.contains("cherry"));
        assert_eq!(result.metadata.as_ref().unwrap()["request_count"], 2);
    }
}
