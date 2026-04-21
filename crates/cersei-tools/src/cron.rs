//! Cron tools: schedule, list, and delete recurring/one-shot tasks.

use super::*;
use serde::{Deserialize, Serialize};

static CRON_REGISTRY: once_cell::sync::Lazy<dashmap::DashMap<String, CronEntry>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEntry {
    pub id: String,
    pub schedule: String,
    pub prompt: String,
    pub created_at: String,
    pub last_run: Option<String>,
    pub run_count: u32,
}

pub fn list_crons() -> Vec<CronEntry> {
    CRON_REGISTRY.iter().map(|e| e.value().clone()).collect()
}

pub fn clear_crons() {
    CRON_REGISTRY.clear();
}

// ─── Cron Tool ───────────────────────────────────────────────────────────────

pub struct CronTool;

#[derive(Debug, Clone, Deserialize)]
pub struct CronRequest {
    pub action: CronAction,
    pub id: Option<String>,
    pub schedule: Option<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CronAction {
    Create,
    List,
    Delete,
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "Cron"
    }
    fn description(&self) -> &str {
        "Manage recurring or one-shot prompt schedules. Use `create` to schedule a prompt, \
        `list` to see all jobs, and `delete` to remove a job."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Shell
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "delete"],
                    "description": "Cron action to perform."
                },
                "id": { "type": "string", "description": "Cron job ID (required for delete)" },
                "schedule": { "type": "string", "description": "Cron expression (e.g. '*/5 * * * *' or 'once:30s') (required for create)" },
                "prompt": { "type": "string", "description": "The prompt to execute (required for create)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let req: CronRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        match req.action {
            CronAction::Create => {
                let schedule = match req.schedule {
                    Some(s) => s,
                    None => return ToolResult::error("`schedule` is required for action `create`"),
                };
                let prompt = match req.prompt {
                    Some(p) => p,
                    None => return ToolResult::error("`prompt` is required for action `create`"),
                };
                let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
                let entry = CronEntry {
                    id: id.clone(),
                    schedule: schedule.clone(),
                    prompt: prompt.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    last_run: None,
                    run_count: 0,
                };
                CRON_REGISTRY.insert(id.clone(), entry);

                ToolResult::success(format!(
                    "Cron job '{}' created: {} → {}",
                    id, schedule, prompt
                ))
            }
            CronAction::List => {
                let entries = list_crons();
                if entries.is_empty() {
                    return ToolResult::success("No cron jobs scheduled.");
                }
                let lines: Vec<String> = entries
                    .iter()
                    .map(|e| {
                        format!(
                            "- [{}] {} → {} (runs: {})",
                            e.id, e.schedule, e.prompt, e.run_count
                        )
                    })
                    .collect();
                ToolResult::success(lines.join("\n"))
            }
            CronAction::Delete => {
                let id = match req.id {
                    Some(id) => id,
                    None => return ToolResult::error("`id` is required for action `delete`"),
                };
                if CRON_REGISTRY.remove(&id).is_some() {
                    ToolResult::success(format!("Cron job '{}' deleted.", id))
                } else {
                    ToolResult::error(format!("Cron job '{}' not found.", id))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;

    fn test_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "cron-test".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    #[tokio::test]
    async fn test_cron_lifecycle() {
        clear_crons();
        let tool = CronTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "create",
                    "schedule": "*/5 * * * *",
                    "prompt": "Run tests"
                }),
                &test_ctx(),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("created"));

        let result = tool.execute(serde_json::json!({"action": "list"}), &test_ctx()).await;
        assert!(result.content.contains("Run tests"));

        let entries = list_crons();
        assert_eq!(entries.len(), 1);
        let id = entries[0].id.clone();

        let result = tool
            .execute(serde_json::json!({"action": "delete", "id": id}), &test_ctx())
            .await;
        assert!(!result.is_error);

        assert!(list_crons().is_empty());
    }
}
