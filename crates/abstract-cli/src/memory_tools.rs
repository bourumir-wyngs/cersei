use async_trait::async_trait;
use cersei_memory::manager::MemoryManager;
use cersei_memory::memdir::MemoryType;
use cersei_tools::{PermissionLevel, Tool, ToolCategory, ToolContext, ToolInfo, ToolResult};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

const MEMORY_RECALL_DESCRIPTION: &str = "Recall relevant memories using the graph memory system. This tool is always available without permission approval. Use this to find prior project conventions, debugging steps, or user preferences.";
const MEMORY_STORE_DESCRIPTION: &str = "Store a durable memory. This tool is always available without permission approval. Use this to remember important project facts, recurring issues, or user preferences. Only stores when the graph feature is active.";

pub fn memory_recall_tool_info() -> ToolInfo {
    ToolInfo {
        name: "MemoryRecall".to_string(),
        description: MEMORY_RECALL_DESCRIPTION.to_string(),
        permission_level: PermissionLevel::None,
        category: ToolCategory::Memory,
    }
}

pub fn memory_store_tool_info() -> ToolInfo {
    ToolInfo {
        name: "MemoryStore".to_string(),
        description: MEMORY_STORE_DESCRIPTION.to_string(),
        permission_level: PermissionLevel::None,
        category: ToolCategory::Memory,
    }
}

pub struct MemoryRecallTool {
    manager: Arc<MemoryManager>,
}

impl MemoryRecallTool {
    pub fn new(manager: Arc<MemoryManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "MemoryRecall"
    }
    fn description(&self) -> &str {
        MEMORY_RECALL_DESCRIPTION
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Memory
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query for relevant memories" },
                "limit": { "type": "integer", "description": "Maximum number of memories to return (default 5)" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            query: String,
            limit: Option<usize>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let results = self.manager.recall(&input.query, input.limit.unwrap_or(5));

        if results.is_empty() {
            return ToolResult::success("No matching memories found.");
        }

        let mut out = String::new();
        for (i, mem) in results.iter().enumerate() {
            out.push_str(&format!("Memory {}:\n{}\n\n", i + 1, mem));
        }

        ToolResult::success(out.trim().to_string())
    }
}

pub struct MemoryStoreTool {
    manager: Arc<MemoryManager>,
}

impl MemoryStoreTool {
    pub fn new(manager: Arc<MemoryManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "MemoryStore"
    }
    fn description(&self) -> &str {
        MEMORY_STORE_DESCRIPTION
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Memory
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The fact or memory to store." },
                "memory_type": { "type": "string", "description": "Type of memory: 'user', 'project', 'reference', 'feedback'. Defaults to 'project'." },
                "confidence": { "type": "number", "description": "Confidence from 0.0 to 1.0 (default 1.0)." }
            },
            "required": ["content"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            content: String,
            memory_type: Option<String>,
            confidence: Option<f32>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let mem_type_str = input.memory_type.as_deref().unwrap_or("project");
        let mem_type = MemoryType::from_str(mem_type_str).unwrap_or(MemoryType::Project);
        let conf = input.confidence.unwrap_or(1.0).clamp(0.0, 1.0);

        if let Some(id) = self.manager.store_memory(&input.content, mem_type, conf) {
            ToolResult::success(format!("Successfully stored memory. ID: {}", id))
        } else {
            ToolResult::success(
                "Failed to store memory (graph backend might be disabled or unavailable).",
            )
        }
    }
}
