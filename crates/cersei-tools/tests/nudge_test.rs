use cersei_tools::file_xgrep::{clear_grep_counters, XGrepTool};
use cersei_tools::file_xmultigrep::XMultiGrepTool;
use cersei_tools::file_xmultiread::XMultiReadTool;
use cersei_tools::file_xread::{clear_read_counters, XReadTool};
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
    let r1 = read_tool
        .execute(json!({"file_path": "test.txt"}), &ctx)
        .await;
    assert!(!r1.content.contains("consider using MultiRead"));

    // Second read - no nudge
    let r2 = read_tool
        .execute(json!({"file_path": "test.txt"}), &ctx)
        .await;
    assert!(!r2.content.contains("consider using MultiRead"));

    // Third read - nudge!
    let r3 = read_tool
        .execute(json!({"file_path": "test.txt"}), &ctx)
        .await;
    assert!(r3.content.contains("consider using MultiRead"));

    // Fourth read - no nudge
    let r4 = read_tool
        .execute(json!({"file_path": "test.txt"}), &ctx)
        .await;
    assert!(!r4.content.contains("consider using MultiRead"));

    // Sixth read - nudge again!
    read_tool
        .execute(json!({"file_path": "test.txt"}), &ctx)
        .await;
    let r6 = read_tool
        .execute(json!({"file_path": "test.txt"}), &ctx)
        .await;
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
    let r = multiread
        .execute(
            json!({
                "requests": [
                    {"file_path": "f1.txt"},
                    {"file_path": "f2.txt"},
                    {"file_path": "f3.txt"}
                ]
            }),
            &ctx,
        )
        .await;

    assert!(!r.is_error);
    assert!(!r.content.contains("consider using MultiRead"));

    // Subsequent single Read should still be count #1 (no nudge)
    let read_tool = XReadTool;
    let r_single = read_tool
        .execute(json!({"file_path": "f1.txt"}), &ctx)
        .await;
    assert!(!r_single.content.contains("consider using MultiRead"));
}

#[tokio::test]
async fn test_grep_nudge_logic() {
    let tmp = tempdir().unwrap();
    let session_id = "grep-nudge-test-session";
    clear_grep_counters(session_id);
    let ctx = test_ctx(tmp.path(), session_id);
    let file_path = tmp.path().join("test.txt");
    std::fs::write(&file_path, "apple\nbanana\ncherry\n").unwrap();

    let grep_tool = XGrepTool;

    // First grep - no nudge
    let r1 = grep_tool
        .execute(json!({"pattern": "apple", "path": "."}), &ctx)
        .await;
    assert!(!r1.content.contains("consider using MultiGrep"));

    // Second grep - no nudge
    let r2 = grep_tool
        .execute(json!({"pattern": "banana", "path": "."}), &ctx)
        .await;
    assert!(!r2.content.contains("consider using MultiGrep"));

    // Third grep - nudge!
    let r3 = grep_tool
        .execute(json!({"pattern": "cherry", "path": "."}), &ctx)
        .await;
    assert!(r3.content.contains("consider using MultiGrep"));

    // Fourth grep - no nudge
    let r4 = grep_tool
        .execute(json!({"pattern": "apple", "path": "."}), &ctx)
        .await;
    assert!(!r4.content.contains("consider using MultiGrep"));

    // Sixth grep - nudge again!
    grep_tool
        .execute(json!({"pattern": "banana", "path": "."}), &ctx)
        .await;
    let r6 = grep_tool
        .execute(json!({"pattern": "cherry", "path": "."}), &ctx)
        .await;
    assert!(r6.content.contains("consider using MultiGrep"));
}

#[tokio::test]
async fn test_multigrep_suppresses_nudge() {
    let tmp = tempdir().unwrap();
    let session_id = "multigrep-nudge-test";
    clear_grep_counters(session_id);
    let ctx = test_ctx(tmp.path(), session_id);
    let f1 = tmp.path().join("f1.txt");
    std::fs::write(&f1, "apple\nbanana\ncherry\n").unwrap();

    let multigrep = XMultiGrepTool;

    // Call MultiGrep with 3 searches. It should NOT trigger the nudge internally.
    let r = multigrep
        .execute(
            json!({
                "requests": [
                    {"pattern": "apple", "path": "."},
                    {"pattern": "banana", "path": "."},
                    {"pattern": "cherry", "path": "."}
                ]
            }),
            &ctx,
        )
        .await;

    assert!(!r.is_error);
    assert!(!r.content.contains("consider using MultiGrep"));

    // Subsequent single Grep should still be count #1 (no nudge)
    let grep_tool = XGrepTool;
    let r_single = grep_tool
        .execute(json!({"pattern": "apple", "path": "."}), &ctx)
        .await;
    assert!(!r_single.content.contains("consider using MultiGrep"));
}
