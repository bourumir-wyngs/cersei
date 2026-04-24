//! Process tool: start long-running bash processes and interact with their output.

use super::*;
use crate::network_policy::{shell_command, NetworkAccess, NetworkDecision};
use crate::permissions::{PermissionDecision, PermissionRequest};
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{sleep, Duration};

const RING_BUFFER_SIZE: usize = 1024;
const DEFAULT_TAIL: usize = 6;

// ─── Per-session process state ───────────────────────────────────────────────

struct ProcessEntry {
    /// The live child handle — held so we can kill it.
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    /// Captured output lines (stdout + stderr interleaved).
    lines: parking_lot::Mutex<VecDeque<String>>,
    /// True while the process is still running.
    running: AtomicBool,
    /// The original command string, for status display.
    command: String,
    /// OS PID of the process (may be 0 if unavailable).
    pid: u32,
}

/// Registry key: (session_id, pid)
static PROCESS_REGISTRY: once_cell::sync::Lazy<dashmap::DashMap<(String, u32), Arc<ProcessEntry>>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);

// ─── Tool ────────────────────────────────────────────────────────────────────

pub struct ProcessTool;

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str {
        "Process"
    }

    fn description(&self) -> &str {
        "Manage long-running bash processes. Always supply the 'action' field. \
        action='start' + command='...' — launch a process, returns its PID. \
        action='list' — list all processes previously started in this session with PID, status, and command \
        action='output' + pid=N — retrieve recent captured output. \
        action='status' + pid=N — check if a process is running. \
        action='kill' + pid=N — terminate a process. \
        Only processes previously started by this tool in the current session can be queried or killed. \
        Multiple processes can run simultaneously. stdout and stderr go to a 1024-line ring buffer."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
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
                    "enum": ["start", "list", "output", "status", "kill"],
                    "default": "list",
                    "description": "Action to perform (default: 'list'). start=launch process, list=show all processes, output=get stdout/stderr, status=check running, kill=terminate."
                },
                "command": {
                    "type": "string",
                    "description": "Bash command to run (required for 'start')"
                },
                "pid": {
                    "type": "integer",
                    "description": "Process PID (required for 'output', 'status', 'kill'). Use 'list' to discover PIDs."
                },
                "tail": {
                    "type": "integer",
                    "description": "Number of recent lines to return for 'output' action (default 6, max 1024)"
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional subdirectory (relative to the working root) in which to start the process. Must not escape the root directory."
                },
                "network": {
                    "type": "string",
                    "enum": ["local", "full"],
                    "description": "Network access for the process (only applies to 'start'). Omit to request normal network access. Use 'local' for local network only (e.g. localhost services). Use 'full' when the process needs external network access."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let raw_input = input.clone();

        #[derive(Deserialize)]
        struct Input {
            action: String,
            command: Option<String>,
            pid: Option<u32>,
            tail: Option<usize>,
            workdir: Option<String>,
            network: Option<String>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        match input.action.as_str() {
            "start" => {
                let command = match input.command {
                    Some(c) => c,
                    None => return ToolResult::error("'command' is required for 'start' action"),
                };
                if let Err(err) = ensure_start_permission(&raw_input, &command, ctx).await {
                    return err;
                }
                let requested = NetworkAccess::from_input(input.network.as_deref());
                start_process(
                    &ctx.session_id,
                    &command,
                    input.workdir.as_deref(),
                    requested,
                    ctx,
                )
                .await
            }
            "list" => list_processes(&ctx.session_id),
            "output" => {
                let pid = match input.pid {
                    Some(p) => p,
                    None => return ToolResult::error("'pid' is required for 'output'. Use action 'list' to see running processes."),
                };
                let tail = input.tail.unwrap_or(DEFAULT_TAIL).min(RING_BUFFER_SIZE);
                get_output(&ctx.session_id, pid, tail)
            }
            "status" => {
                let pid = match input.pid {
                    Some(p) => p,
                    None => return ToolResult::error("'pid' is required for 'status'. Use action 'list' to see running processes."),
                };
                get_status(&ctx.session_id, pid)
            }
            "kill" => {
                let pid = match input.pid {
                    Some(p) => p,
                    None => return ToolResult::error(
                        "'pid' is required for 'kill'. Use action 'list' to see running processes.",
                    ),
                };
                kill_process(&ctx.session_id, pid).await
            }
            other => ToolResult::error(format!(
                "Unknown action: '{}'. Use start, list, output, status, or kill.",
                other
            )),
        }
    }
}

// ─── Action implementations ──────────────────────────────────────────────────

async fn ensure_start_permission(
    tool_input: &Value,
    command: &str,
    ctx: &ToolContext,
) -> std::result::Result<(), ToolResult> {
    let preview = if command.len() > 80 {
        format!("{}…", &command[..79])
    } else {
        command.to_string()
    };

    let request = PermissionRequest {
        tool_name: "Process".into(),
        tool_input: tool_input.clone(),
        permission_level: PermissionLevel::Execute,
        description: format!("Start process: {}", preview),
        id: format!("process-start-{}", uuid::Uuid::new_v4()),
        working_dir: ctx.working_dir.clone(),
    };

    match ctx.permissions.check(&request).await {
        PermissionDecision::Allow
        | PermissionDecision::AllowOnce
        | PermissionDecision::AllowForSession => Ok(()),
        PermissionDecision::Deny(reason) => {
            Err(ToolResult::error(format!("Permission denied: {}", reason)))
        }
    }
}

async fn start_process(
    session_id: &str,
    command: &str,
    workdir: Option<&str>,
    requested: NetworkAccess,
    ctx: &ToolContext,
) -> ToolResult {
    let shell_state = session_shell_state(session_id);
    let (base_cwd, env_vars) = {
        let state = shell_state.lock();
        (
            state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone()),
            state.env_vars.clone(),
        )
    };

    let cwd = if let Some(dir) = workdir {
        let candidate = PathBuf::from(&base_cwd).join(dir);
        let canonical_root = match ctx.working_dir.canonicalize() {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Cannot resolve working root: {}", e)),
        };
        let canonical_candidate = match candidate.canonicalize() {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Cannot resolve workdir '{}': {}", dir, e)),
        };
        if !canonical_candidate.starts_with(&canonical_root) {
            return ToolResult::error(format!(
                "workdir '{}' is outside the allowed root '{}'",
                dir,
                canonical_root.display()
            ));
        }
        canonical_candidate
    } else {
        base_cwd
    };

    let access = match ctx.network_policy {
        Some(ref policy) => match policy.check("Process", command, requested).await {
            NetworkDecision::Allow(access) => access,
            NetworkDecision::Deny(reason) => {
                return ToolResult::error(format!("Permission denied: {}", reason));
            }
        },
        None => requested,
    };

    let mut cmd = match shell_command(command, access) {
        Ok(cmd) => cmd,
        Err(err) => return ToolResult::error(err),
    };
    cmd.current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for (k, v) in &env_vars {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("Failed to start process: {}", e)),
    };

    let pid = child.id().unwrap_or(0);

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let entry = Arc::new(ProcessEntry {
        child: tokio::sync::Mutex::new(Some(child)),
        lines: parking_lot::Mutex::new(VecDeque::new()),
        running: AtomicBool::new(true),
        command: command.to_string(),
        pid,
    });

    PROCESS_REGISTRY.insert((session_id.to_string(), pid), Arc::clone(&entry));

    // Spawn stdout reader
    let entry_stdout = Arc::clone(&entry);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            push_buffer_line(&entry_stdout, line);
        }
    });

    // Spawn stderr reader
    let entry_stderr = Arc::clone(&entry);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            push_buffer_line(&entry_stderr, format!("[stderr] {}", line));
        }
    });

    // Poll child state without holding the child lock across await points so
    // explicit `kill` requests can still acquire the handle.
    let entry_watcher = Arc::clone(&entry);
    tokio::spawn(async move {
        loop {
            let state = {
                let mut guard = entry_watcher.child.lock().await;
                let Some(child) = guard.as_mut() else {
                    break;
                };

                match child.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        Some(Ok(status))
                    }
                    Ok(None) => None,
                    Err(err) => {
                        *guard = None;
                        Some(Err(err.to_string()))
                    }
                }
            };

            match state {
                Some(Ok(status)) => {
                    entry_watcher.running.store(false, Ordering::SeqCst);
                    push_exit_line(&entry_watcher, Some(status));
                    break;
                }
                Some(Err(err)) => {
                    entry_watcher.running.store(false, Ordering::SeqCst);
                    push_buffer_line(&entry_watcher, format!("[process wait failed: {}]", err));
                    break;
                }
                None => sleep(Duration::from_millis(250)).await,
            }
        }
    });

    ToolResult::success(format!("Process started (pid {}): {}", pid, command))
}

fn push_buffer_line(entry: &ProcessEntry, line: impl Into<String>) {
    let mut buf = entry.lines.lock();
    if buf.len() >= RING_BUFFER_SIZE {
        buf.pop_front();
    }
    buf.push_back(line.into());
}

fn push_exit_line(entry: &ProcessEntry, status: Option<std::process::ExitStatus>) {
    let exit_msg = match status {
        Some(s) => format!("[process exited: {}]", s),
        None => "[process exited]".to_string(),
    };
    push_buffer_line(entry, exit_msg);
}

fn lookup_process(
    session_id: &str,
    pid: u32,
) -> std::result::Result<Arc<ProcessEntry>, ToolResult> {
    PROCESS_REGISTRY
        .get(&(session_id.to_string(), pid))
        .map(|entry| Arc::clone(entry.value()))
        .ok_or_else(|| {
            ToolResult::error(format!(
                "No process with pid {} is managed by this session. Use action 'list' to see processes started by this tool.",
                pid
            ))
        })
}

fn list_processes(session_id: &str) -> ToolResult {
    let mut entries: Vec<(u32, Arc<ProcessEntry>)> = PROCESS_REGISTRY
        .iter()
        .filter(|e| e.key().0 == session_id)
        .map(|e| (e.key().1, Arc::clone(e.value())))
        .collect();

    if entries.is_empty() {
        return ToolResult::success("No processes");
    }

    entries.sort_by_key(|(pid, _)| *pid);

    let lines: Vec<String> = entries
        .iter()
        .map(|(pid, entry)| {
            let status = if entry.running.load(Ordering::SeqCst) {
                "running"
            } else {
                "exited"
            };
            format!(
                "pid={:<8} status={:<8} command={}",
                pid, status, entry.command
            )
        })
        .collect();

    ToolResult::success(lines.join("\n"))
}

fn get_output(session_id: &str, pid: u32, tail: usize) -> ToolResult {
    let entry = match lookup_process(session_id, pid) {
        Ok(entry) => entry,
        Err(err) => return err,
    };

    let buf = entry.lines.lock();
    let skip = buf.len().saturating_sub(tail);
    let lines: Vec<&str> = buf.iter().skip(skip).map(String::as_str).collect();
    if lines.is_empty() {
        ToolResult::success("(no output yet)")
    } else {
        ToolResult::success(lines.join("\n"))
    }
}

fn get_status(session_id: &str, pid: u32) -> ToolResult {
    let entry = match lookup_process(session_id, pid) {
        Ok(entry) => entry,
        Err(err) => return err,
    };

    let running = entry.running.load(Ordering::SeqCst);
    let line_count = entry.lines.lock().len();
    ToolResult::success(format!(
        "pid: {}\ncommand: {}\nrunning: {}\nbuffered lines: {}",
        entry.pid, entry.command, running, line_count
    ))
}

async fn kill_process(session_id: &str, pid: u32) -> ToolResult {
    let entry = match lookup_process(session_id, pid) {
        Ok(entry) => entry,
        Err(err) => return err,
    };

    let mut guard = entry.child.lock().await;
    if let Some(child) = guard.as_mut() {
        match child.kill().await {
            Ok(_) => {
                let status = child.try_wait().ok().flatten();
                *guard = None;
                entry.running.store(false, Ordering::SeqCst);
                push_exit_line(&entry, status);
                drop(guard);
                ToolResult::success(format!("Process {} killed", pid))
            }
            Err(e) => ToolResult::error(format!("Failed to kill process: {}", e)),
        }
    } else {
        ToolResult::error("Process already exited")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_policy::{NetworkDecision, NetworkPolicy};
    use crate::permissions::PermissionPolicy;
    use parking_lot::Mutex;
    use serde_json::json;

    struct RecordingPermissionPolicy {
        decision: PermissionDecision,
        requests: Mutex<Vec<PermissionRequest>>,
    }

    impl RecordingPermissionPolicy {
        fn new(decision: PermissionDecision) -> Self {
            Self {
                decision,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().len()
        }
    }

    #[async_trait]
    impl PermissionPolicy for RecordingPermissionPolicy {
        async fn check(&self, request: &PermissionRequest) -> PermissionDecision {
            self.requests.lock().push(request.clone());
            self.decision.clone()
        }
    }

    struct FixedNetworkPolicy {
        decision: NetworkDecision,
    }

    #[async_trait]
    impl NetworkPolicy for FixedNetworkPolicy {
        async fn check(
            &self,
            _tool_name: &str,
            _command: &str,
            requested: NetworkAccess,
        ) -> NetworkDecision {
            match &self.decision {
                NetworkDecision::Allow(NetworkAccess::Full)
                | NetworkDecision::Allow(NetworkAccess::Local)
                | NetworkDecision::Allow(NetworkAccess::Blocked) => {
                    NetworkDecision::Allow(requested)
                }
                NetworkDecision::Deny(reason) => NetworkDecision::Deny(reason.clone()),
            }
        }
    }

    fn test_ctx(
        session_id: String,
        permissions: Arc<dyn PermissionPolicy>,
        network_policy: Option<Arc<dyn NetworkPolicy>>,
    ) -> ToolContext {
        ToolContext {
            session_id,
            permissions,
            working_dir: std::env::current_dir().expect("cwd"),
            network_policy,
            ..ToolContext::default()
        }
    }

    fn extract_pid(result: &ToolResult) -> u32 {
        let start = result.content.find("(pid ").expect("pid marker") + 5;
        let end = result.content[start..].find(')').expect("pid end") + start;
        result.content[start..end].parse().expect("numeric pid")
    }

    // These tests use unique session ids, so clearing the shared registry would
    // race with other concurrently running process_tool tests.

    #[tokio::test]
    async fn start_requires_permission_but_follow_up_actions_do_not() {
        let policy = Arc::new(RecordingPermissionPolicy::new(PermissionDecision::Allow));
        let ctx = test_ctx(
            format!("process-test-{}", uuid::Uuid::new_v4()),
            policy.clone(),
            None,
        );
        let tool = ProcessTool;

        let start = tool
            .execute(
                json!({
                    "action": "start",
                    "command": "printf 'hello\\n'; sleep 30"
                }),
                &ctx,
            )
            .await;

        assert!(!start.is_error, "{}", start.content);
        assert_eq!(policy.request_count(), 1);

        let pid = extract_pid(&start);

        let status = tool
            .execute(json!({"action": "status", "pid": pid}), &ctx)
            .await;
        assert!(!status.is_error, "{}", status.content);

        let output = tool
            .execute(json!({"action": "output", "pid": pid, "tail": 4}), &ctx)
            .await;
        assert!(!output.is_error, "{}", output.content);

        let list = tool.execute(json!({"action": "list"}), &ctx).await;
        assert!(!list.is_error, "{}", list.content);

        let kill = tool
            .execute(json!({"action": "kill", "pid": pid}), &ctx)
            .await;
        assert!(!kill.is_error, "{}", kill.content);

        assert_eq!(policy.request_count(), 1);
    }

    #[tokio::test]
    async fn start_is_blocked_when_permission_is_denied() {
        let ctx = test_ctx(
            format!("process-test-{}", uuid::Uuid::new_v4()),
            Arc::new(RecordingPermissionPolicy::new(PermissionDecision::Deny(
                "User denied".into(),
            ))),
            None,
        );
        let tool = ProcessTool;

        let result = tool
            .execute(
                json!({
                    "action": "start",
                    "command": "sleep 1"
                }),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Permission denied: User denied"));
        assert_eq!(list_processes(&ctx.session_id).content, "No processes");
    }

    #[tokio::test]
    async fn other_sessions_cannot_query_or_kill_started_processes() {
        let owner_ctx = test_ctx(
            format!("process-owner-{}", uuid::Uuid::new_v4()),
            Arc::new(RecordingPermissionPolicy::new(PermissionDecision::Allow)),
            None,
        );
        let other_ctx = test_ctx(
            format!("process-other-{}", uuid::Uuid::new_v4()),
            Arc::new(RecordingPermissionPolicy::new(PermissionDecision::Allow)),
            None,
        );
        let tool = ProcessTool;

        let start = tool
            .execute(
                json!({
                    "action": "start",
                    "command": "sleep 30"
                }),
                &owner_ctx,
            )
            .await;
        assert!(!start.is_error, "{}", start.content);

        let pid = extract_pid(&start);

        let status = tool
            .execute(json!({"action": "status", "pid": pid}), &other_ctx)
            .await;
        assert!(status.is_error);
        assert!(status.content.contains("No process with pid"));

        let output = tool
            .execute(json!({"action": "output", "pid": pid}), &other_ctx)
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("No process with pid"));

        let kill = tool
            .execute(json!({"action": "kill", "pid": pid}), &other_ctx)
            .await;
        assert!(kill.is_error);
        assert!(kill.content.contains("No process with pid"));

        let list = tool.execute(json!({"action": "list"}), &other_ctx).await;
        assert!(!list.is_error);
        assert_eq!(list.content, "No processes");

        let owner_kill = tool
            .execute(json!({"action": "kill", "pid": pid}), &owner_ctx)
            .await;
        assert!(!owner_kill.is_error, "{}", owner_kill.content);
    }

    #[tokio::test]
    async fn start_is_blocked_when_network_is_denied() {
        let ctx = test_ctx(
            format!("process-test-{}", uuid::Uuid::new_v4()),
            Arc::new(RecordingPermissionPolicy::new(PermissionDecision::Allow)),
            Some(Arc::new(FixedNetworkPolicy {
                decision: NetworkDecision::Deny("User denied (registered rule)".into()),
            })),
        );
        let tool = ProcessTool;

        let result = tool
            .execute(
                json!({
                    "action": "start",
                    "command": "sleep 1",
                    "network": "full"
                }),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("Permission denied: User denied (registered rule)"));
        assert_eq!(list_processes(&ctx.session_id).content, "No processes");
    }
}
