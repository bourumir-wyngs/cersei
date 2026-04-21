//! Task system: create, track, update, and manage background tasks.
//!
//! Tasks represent long-running sub-agent work that runs asynchronously.
//! The coordinator can create tasks, check their status, and retrieve output.

use super::*;
use serde::{Deserialize, Serialize};

// ─── Task registry ───────────────────────────────────────────────────────────

static TASK_REGISTRY: once_cell::sync::Lazy<dashmap::DashMap<String, TaskEntry>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEntry {
    pub id: String,
    pub description: String,
    pub status: TaskStatus,
    pub output: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Stopped,
}

pub fn get_task(id: &str) -> Option<TaskEntry> {
    TASK_REGISTRY.get(id).map(|e| e.clone())
}

pub fn list_tasks() -> Vec<TaskEntry> {
    TASK_REGISTRY.iter().map(|e| e.value().clone()).collect()
}

pub fn clear_tasks() {
    TASK_REGISTRY.clear();
}

// ─── Tasks Tool ──────────────────────────────────────────────────────────────

pub struct TasksTool;

#[derive(Debug, Clone, Deserialize)]
pub struct TasksRequest {
    pub action: TasksAction,
    pub id: Option<String>,
    pub description: Option<String>,
    pub status: Option<TaskStatus>,
    pub output: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TasksAction {
    Create,
    Get,
    Update,
    List,
    Stop,
    Output,
}

#[async_trait]
impl Tool for TasksTool {
    fn name(&self) -> &str {
        "Tasks"
    }

    fn description(&self) -> &str {
        "Manage background tasks. Use `create` to start tracking sub-agent work, \
        `get` to check status, `update` to set status or output, `list` to see all tasks, \
        `stop` to cancel a task, and `output` to retrieve full result."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Orchestration
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "get", "update", "list", "stop", "output"],
                    "description": "Task action to perform."
                },
                "id": { "type": "string", "description": "Task ID (required for get, update, stop, output)" },
                "description": { "type": "string", "description": "Task description (required for create)" },
                "status": {
                    "type": "string",
                    "enum": ["pending", "running", "completed", "failed", "stopped"],
                    "description": "New status for update"
                },
                "output": { "type": "string", "description": "Task output for update" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: TasksRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        match req.action {
            TasksAction::Create => {
                let description = match req.description {
                    Some(d) => d,
                    None => return ToolResult::error("`description` is required for action `create`"),
                };
                let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
                let now = chrono::Utc::now().to_rfc3339();
                let task = TaskEntry {
                    id: id.clone(),
                    description: description.clone(),
                    status: TaskStatus::Pending,
                    output: None,
                    created_at: now.clone(),
                    updated_at: now,
                    session_id: ctx.session_id.clone(),
                };
                TASK_REGISTRY.insert(id.clone(), task);
                ToolResult::success(format!("Task '{}' created: {}", id, description))
            }
            TasksAction::Get => {
                let id = match req.id {
                    Some(id) => id,
                    None => return ToolResult::error("`id` is required for action `get`"),
                };
                match get_task(&id) {
                    Some(task) => {
                        let output = task.output.as_deref().unwrap_or("(no output yet)");
                        ToolResult::success(format!(
                            "Task [{}] {:?}\n  {}\n  Output: {}",
                            task.id, task.status, task.description, output
                        ))
                    }
                    None => ToolResult::error(format!("Task '{}' not found", id)),
                }
            }
            TasksAction::Update => {
                let id = match req.id {
                    Some(id) => id,
                    None => return ToolResult::error("`id` is required for action `update`"),
                };
                match TASK_REGISTRY.get_mut(&id) {
                    Some(mut entry) => {
                        if let Some(status) = req.status {
                            entry.status = status;
                        }
                        if let Some(output) = req.output {
                            entry.output = Some(output);
                        }
                        entry.updated_at = chrono::Utc::now().to_rfc3339();
                        ToolResult::success(format!("Task '{}' updated", id))
                    }
                    None => ToolResult::error(format!("Task '{}' not found", id)),
                }
            }
            TasksAction::List => {
                let tasks = list_tasks();
                if tasks.is_empty() {
                    return ToolResult::success("No tasks.");
                }
                let lines: Vec<String> = tasks
                    .iter()
                    .map(|t| {
                        let status = format!("{:?}", t.status);
                        format!("- [{}] {} — {}", t.id, status, t.description)
                    })
                    .collect();
                ToolResult::success(lines.join("\n"))
            }
            TasksAction::Stop => {
                let id = match req.id {
                    Some(id) => id,
                    None => return ToolResult::error("`id` is required for action `stop`"),
                };
                match TASK_REGISTRY.get_mut(&id) {
                    Some(mut entry) => {
                        entry.status = TaskStatus::Stopped;
                        entry.updated_at = chrono::Utc::now().to_rfc3339();
                        ToolResult::success(format!("Task '{}' stopped", id))
                    }
                    None => ToolResult::error(format!("Task '{}' not found", id)),
                }
            }
            TasksAction::Output => {
                let id = match req.id {
                    Some(id) => id,
                    None => return ToolResult::error("`id` is required for action `output`"),
                };
                match get_task(&id) {
                    Some(task) => match &task.output {
                        Some(output) => ToolResult::success(output.clone()),
                        None => ToolResult::success("(no output yet)"),
                    },
                    None => ToolResult::error(format!("Task '{}' not found", id)),
                }
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;

    fn test_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "task-test".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    #[tokio::test]
    async fn test_task_full_lifecycle() {
        clear_tasks();
        let ctx = ToolContext {
            session_id: format!("task-lifecycle-{}", uuid::Uuid::new_v4()),
            ..test_ctx()
        };

        let tool = TasksTool;

        // Create
        let r = tool
            .execute(serde_json::json!({"action": "create", "description": "Run tests"}), &ctx)
            .await;
        assert!(!r.is_error);
        let id = r.content.split('\'').nth(1).unwrap().to_string();

        // List
        let r = tool.execute(serde_json::json!({"action": "list"}), &ctx).await;
        assert!(r.content.contains("Run tests"));

        // Update to running
        tool.execute(serde_json::json!({"action": "update", "id": &id, "status": "running"}), &ctx)
            .await;
        assert_eq!(get_task(&id).unwrap().status, TaskStatus::Running);

        // Update with output
        tool.execute(
                serde_json::json!({
                    "action": "update",
                    "id": &id,
                    "status": "completed",
                    "output": "All 42 tests passed"
                }),
                &ctx,
            )
            .await;
        let task = get_task(&id).unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.output.as_deref(), Some("All 42 tests passed"));

        // Get output
        let r = tool.execute(serde_json::json!({"action": "output", "id": &id}), &ctx).await;
        assert!(r.content.contains("42 tests passed"));

        // Get status
        let r = tool.execute(serde_json::json!({"action": "get", "id": &id}), &ctx).await;
        assert!(r.content.contains("Completed"));
    }

    #[tokio::test]
    async fn test_task_stop() {
        let ctx = ToolContext {
            session_id: format!("stop-{}", uuid::Uuid::new_v4()),
            ..test_ctx()
        };

        let tool = TasksTool;
        let r = tool
            .execute(serde_json::json!({"action": "create", "description": "Long task"}), &ctx)
            .await;
        let id = r.content.split('\'').nth(1).unwrap().to_string();

        tool.execute(serde_json::json!({"action": "stop", "id": &id}), &ctx).await;
        assert_eq!(get_task(&id).unwrap().status, TaskStatus::Stopped);
    }
}
