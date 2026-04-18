use crate::xfile_storage::{
    diff_against_checkpoint, diff_files, xfile_session_id, XCheckpointDiffSummary,
};
use crate::{
    PermissionLevel, ReviewRequest, ReviewService, Tool, ToolCategory, ToolContext, ToolResult,
};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ReviewTool;

#[async_trait]
impl Tool for ReviewTool {
    fn name(&self) -> &str {
        "Review"
    }

    fn description(&self) -> &str {
        "Send the tracked diff since the latest checkpoint to the reviewer agent. The reviewer has its own session history, shares workspace XFileStorage, and returns major findings about defects, unsafe code, or suspicious intent."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Custom
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        if input != json!({}) {
            return ToolResult::error("Review does not accept any arguments.");
        }

        let Some(service) = ctx.extensions.get::<ReviewService>() else {
            return ToolResult::error("Reviewer service is not available in this session.");
        };

        let storage_session_id = xfile_session_id(ctx);
        let summary = match diff_against_checkpoint(&storage_session_id) {
            Ok(summary) => summary,
            Err(err) => return ToolResult::error(err),
        };

        if summary.entries.is_empty() {
            return ToolResult::success(
                "Reviewer feedback: no tracked changes since the latest checkpoint.",
            );
        }

        let diff = render_changes(&summary);
        let response = match service.review(ReviewRequest::checkpoint(diff)).await {
            Ok(response) => response,
            Err(err) => return ToolResult::error(format!("Reviewer failed: {err}")),
        };

        ToolResult::success(format_review_response(&response))
    }
}

fn format_review_response(response: &crate::ReviewResponse) -> String {
    format!(
        "Reviewer feedback from session {} using {}:\n\n{}",
        response.reviewer_session_id, response.reviewer_model, response.review
    )
}

fn render_changes(summary: &XCheckpointDiffSummary) -> String {
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

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::{Extensions, ReviewExecutor, ReviewRequest, ReviewResponse};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct FakeReviewer;

    #[async_trait]
    impl ReviewExecutor for FakeReviewer {
        async fn review(
            &self,
            _request: ReviewRequest,
        ) -> std::result::Result<ReviewResponse, String> {
            Ok(ReviewResponse {
                review: "serious issue".to_string(),
                reviewer_model: "openai/gpt-5.4".to_string(),
                reviewer_session_id: "reviewer-session".to_string(),
            })
        }
    }

    fn test_ctx() -> ToolContext {
        let extensions = Extensions::default();
        extensions.insert(ReviewService::new(Arc::new(FakeReviewer)));
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "review-tool-test".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(crate::CostTracker::new()),
            mcp_manager: None,
            extensions,
            network_policy: None,
        }
    }

    #[test]
    fn format_review_response_mentions_session_and_model() {
        let text = format_review_response(&ReviewResponse {
            review: "bad bug".to_string(),
            reviewer_model: "anthropic/claude-sonnet-4-6".to_string(),
            reviewer_session_id: "abc-reviewer".to_string(),
        });

        assert!(text.contains("abc-reviewer"));
        assert!(text.contains("claude-sonnet-4-6"));
        assert!(text.contains("bad bug"));
    }

    #[tokio::test]
    async fn rejects_arguments() {
        let result = ReviewTool
            .execute(json!({"unexpected": true}), &test_ctx())
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("does not accept any arguments"));
    }
}
