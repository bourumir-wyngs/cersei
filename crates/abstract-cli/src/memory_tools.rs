use cersei_tools::{Tool, ToolContext, PermissionLevel, ToolResult};
use serde::Deserialize;
use serde_json::Value;
use async_trait::async_trait;
use cersei_memory::manager::MemoryManager;
use cersei_memory::memdir::MemoryType;
use std::sync::Arc;

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
    fn name(&self) -> &str { "MemoryRecall" }
    fn description(&self) -> &str {
        "Recall relevant memories using the graph memory system. This tool is always available without permission approval. Use this to find prior project conventions, debugging steps, or user preferences."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
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
    fn name(&self) -> &str { "MemoryStore" }
    fn description(&self) -> &str {
        "Store a durable memory. This tool is always available without permission approval. Use this to remember important project facts, recurring issues, or user preferences. Only stores when the graph feature is active."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
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
            ToolResult::success("Failed to store memory (graph backend might be disabled or unavailable).")
        }
    }
}
