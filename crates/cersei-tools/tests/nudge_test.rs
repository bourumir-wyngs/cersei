use cersei_tools::file_xread::{clear_read_counters, XReadTool};
use cersei_tools::file_xmultiread::XMultiReadTool;
use cersei_tools::permissions::AllowAll;
use cersei_tools::{CostTracker, Extensions, Tool, ToolContext};
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;

fn test_ctx(root: &std::path::Path, session_id: &str) -> ToolContext {
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
async fn test_read_nudge_logic() {
    let tmp = tempdir().unwrap();
    let session_id = "nudge-test-session";
    clear_read_counters(session_id);
    let ctx = test_ctx(tmp.path(), session_id);
    let file_path = tmp.path().join("test.txt");
    std::fs::write(&file_path, "line1\nline2\n").unwrap();

    let read_tool = XReadTool;

    // First read - no nudge
    let r1 = read_tool.execute(json!({"file_path": "test.txt"}), &ctx).await;
    assert!(!r1.content.contains("consider using MultiRead"));

    // Second read - no nudge
    let r2 = read_tool.execute(json!({"file_path": "test.txt"}), &ctx).await;
    assert!(!r2.content.contains("consider using MultiRead"));

    // Third read - nudge!
    let r3 = read_tool.execute(json!({"file_path": "test.txt"}), &ctx).await;
    assert!(r3.content.contains("consider using MultiRead"));

    // Fourth read - no nudge
    let r4 = read_tool.execute(json!({"file_path": "test.txt"}), &ctx).await;
    assert!(!r4.content.contains("consider using MultiRead"));

    // Sixth read - nudge again!
    read_tool.execute(json!({"file_path": "test.txt"}), &ctx).await;
    let r6 = read_tool.execute(json!({"file_path": "test.txt"}), &ctx).await;
    assert!(r6.content.contains("consider using MultiRead"));
}

#[tokio::test]
async fn test_multiread_suppresses_nudge() {
    let tmp = tempdir().unwrap();
    let session_id = "multiread-nudge-test";
    clear_read_counters(session_id);
    let ctx = test_ctx(tmp.path(), session_id);
    let f1 = tmp.path().join("f1.txt");
    let f2 = tmp.path().join("f2.txt");
    let f3 = tmp.path().join("f3.txt");
    std::fs::write(&f1, "1").unwrap();
    std::fs::write(&f2, "2").unwrap();
    std::fs::write(&f3, "3").unwrap();

    let multiread = XMultiReadTool;

    // Call MultiRead with 3 files. It should NOT trigger the nudge internally
    // and should NOT increment the session counter in a way that affects future Read calls.
    let r = multiread.execute(json!({
        "requests": [
            {"file_path": "f1.txt"},
            {"file_path": "f2.txt"},
            {"file_path": "f3.txt"}
        ]
    }), &ctx).await;

    assert!(!r.is_error);
    assert!(!r.content.contains("consider using MultiRead"));

    // Subsequent single Read should still be count #1 (no nudge)
    let read_tool = XReadTool;
    let r_single = read_tool.execute(json!({"file_path": "f1.txt"}), &ctx).await;
    assert!(!r_single.content.contains("consider using MultiRead"));
}
