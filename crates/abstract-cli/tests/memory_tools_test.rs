#[cfg(test)]
mod tests {
    use cersei_memory::manager::MemoryManager;
    use cersei_tools::{Tool, ToolContext};
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::tempdir;

    use abstract_cli::memory_tools::{MemoryRecallTool, MemoryStoreTool};

    #[tokio::test]
    async fn test_store_and_recall_memory_fallback() {
        let dir = tempdir().unwrap();
        let mm = Arc::new(MemoryManager::new(dir.path()));

        let store = MemoryStoreTool::new(mm.clone());
        let recall = MemoryRecallTool::new(mm.clone());

        let ctx = ToolContext::default();

        // 1. Without graph backend, store succeeds with graceful message
        let store_res = store
            .execute(
                json!({
                    "content": "The database port is 5433.",
                    "memory_type": "project"
                }),
                &ctx,
            )
            .await;
        let store_out = store_res.content;
        assert!(
            store_out.contains("might be disabled or unavailable"),
            "got: {}",
            store_out
        );

        // 2. Recall works with graceful message or empty
        let recall_res = recall
            .execute(
                json!({
                    "query": "database port",
                    "limit": 5
                }),
                &ctx,
            )
            .await;
        assert!(
            recall_res.content.contains("No matching memories"),
            "got: {}",
            recall_res.content
        );
    }
}
