//! Process tool: start long-running bash processes and interact with their output.

use super::*;
use crate::network_policy::{shell_command, NetworkAccess};
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

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
static PROCESS_REGISTRY: once_cell::sync::Lazy<
    dashmap::DashMap<(String, u32), Arc<ProcessEntry>>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

// ─── Tool ────────────────────────────────────────────────────────────────────

pub struct ProcessTool;

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str { "Process" }

    fn description(&self) -> &str {
        "Start and manage long-running bash processes. Multiple processes can run simultaneously. \
        Actions: \
        'start' — launch a process, returns its PID; \
        'list' — list all processes for this session with PID, status, and command; \
        'output' — retrieve recent captured output (requires pid); \
        'status' — check if a process is running (requires pid); \
        'kill' — terminate a process (requires pid). \
        stdout and stderr are captured into a 1024-line ring buffer per process. \
        Use 'list' to discover PIDs, then use pid to target a specific process."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::Execute }
    fn category(&self) -> ToolCategory { ToolCategory::Shell }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "list", "output", "status", "kill"],
                    "description": "Action to perform. Use 'list' to see all processes and their PIDs."
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
                    "enum": ["none", "local", "full"],
                    "description": "Network access for the process (only applies to 'start'). Default: none (sandboxed, no network). Use 'local' for local network only (e.g. localhost services). Use 'full' when the process needs external network access."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
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
                let requested = NetworkAccess::from_input(input.network.as_deref());
                start_process(&ctx.session_id, &command, input.workdir.as_deref(), requested, ctx).await
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
                    None => return ToolResult::error("'pid' is required for 'kill'. Use action 'list' to see running processes."),
                };
                kill_process(&ctx.session_id, pid).await
            }
            other => ToolResult::error(format!("Unknown action: '{}'. Use start, list, output, status, or kill.", other)),
        }
    }
}

// ─── Action implementations ──────────────────────────────────────────────────

async fn start_process(session_id: &str, command: &str, workdir: Option<&str>, requested: NetworkAccess, ctx: &ToolContext) -> ToolResult {
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
        Some(ref policy) => policy.check("Process", command, requested).await,
        None => requested,
    };

    let mut cmd = shell_command(command, access);
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
            let mut buf = entry_stdout.lines.lock();
            if buf.len() >= RING_BUFFER_SIZE {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    });

    // Spawn stderr reader
    let entry_stderr = Arc::clone(&entry);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let mut buf = entry_stderr.lines.lock();
            if buf.len() >= RING_BUFFER_SIZE {
                buf.pop_front();
            }
            buf.push_back(format!("[stderr] {}", line));
        }
    });

    // Spawn a watcher that marks the process as done when it exits
    let entry_watcher = Arc::clone(&entry);
    tokio::spawn(async move {
        let status = {
            let mut guard = entry_watcher.child.lock().await;
            if let Some(child) = guard.as_mut() {
                child.wait().await.ok()
            } else {
                None
            }
        };
        entry_watcher.running.store(false, Ordering::SeqCst);
        let exit_msg = match status {
            Some(s) => format!("[process exited: {}]", s),
            None => "[process exited]".to_string(),
        };
        let mut buf = entry_watcher.lines.lock();
        if buf.len() >= RING_BUFFER_SIZE {
            buf.pop_front();
        }
        buf.push_back(exit_msg);
        // Entry stays in registry so output/status remain accessible after exit.
    });

    ToolResult::success(format!("Process started (pid {}): {}", pid, command))
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
            let status = if entry.running.load(Ordering::SeqCst) { "running" } else { "exited" };
            format!("pid={:<8} status={:<8} command={}", pid, status, entry.command)
        })
        .collect();

    ToolResult::success(lines.join("\n"))
}

fn get_output(session_id: &str, pid: u32, tail: usize) -> ToolResult {
    let entry = match PROCESS_REGISTRY.get(&(session_id.to_string(), pid)) {
        Some(e) => Arc::clone(e.value()),
        None => return ToolResult::error(format!("No process with pid {}. Use action 'list' to see processes.", pid)),
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
    let entry = match PROCESS_REGISTRY.get(&(session_id.to_string(), pid)) {
        Some(e) => Arc::clone(e.value()),
        None => return ToolResult::error(format!("No process with pid {}. Use action 'list' to see processes.", pid)),
    };

    let running = entry.running.load(Ordering::SeqCst);
    let line_count = entry.lines.lock().len();
    ToolResult::success(format!(
        "pid: {}\ncommand: {}\nrunning: {}\nbuffered lines: {}",
        entry.pid, entry.command, running, line_count
    ))
}

async fn kill_process(session_id: &str, pid: u32) -> ToolResult {
    let entry = match PROCESS_REGISTRY.get(&(session_id.to_string(), pid)) {
        Some(e) => Arc::clone(e.value()),
        None => return ToolResult::error(format!("No process with pid {}. Use action 'list' to see processes.", pid)),
    };

    let mut guard = entry.child.lock().await;
    if let Some(child) = guard.as_mut() {
        match child.kill().await {
            Ok(_) => {
                drop(guard);
                ToolResult::success(format!("Process {} killed", pid))
            }
            Err(e) => ToolResult::error(format!("Failed to kill process: {}", e)),
        }
    } else {
        ToolResult::error("Process already exited")
    }
}
