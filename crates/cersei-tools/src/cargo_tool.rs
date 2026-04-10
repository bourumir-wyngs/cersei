//! Cargo tool: run cargo commands.

use super::*;
use crate::network_policy::{shell_command, NetworkAccess};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;

pub struct CargoTool;

#[async_trait]
impl Tool for CargoTool {
    fn name(&self) -> &str {
        "Cargo"
    }

    fn description(&self) -> &str {
        "Run a cargo command (e.g. build, test, run, check, clippy, fmt, publish). \
        The working directory persists between commands."
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
                "args": {
                    "type": "string",
                    "description": "Arguments to pass to cargo, e.g. \"build --release\", \"test\", \"clippy -- -D warnings\""
                },
                "directory": {
                    "type": "string",
                    "description": "Optional subdirectory (relative to the working root) in which to run the command. Must not escape the root directory."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (max 600000)"
                },
                "network": {
                    "type": "string",
                    "enum": ["local", "full"],
                    "description": "Network access required. Omit to request normal network access. Use 'local' for local network only. Use 'full' when cargo needs to fetch crates."
                }
            },
            "required": ["args"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            args: String,
            directory: Option<String>,
            timeout: Option<u64>,
            network: Option<String>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let shell_state = session_shell_state(&ctx.session_id);
        let base_cwd = {
            let state = shell_state.lock();
            state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone())
        };

        let cwd = if let Some(dir) = input.directory {
            let candidate = PathBuf::from(&base_cwd).join(&dir);
            let canonical_root = match ctx.working_dir.canonicalize() {
                Ok(p) => p,
                Err(e) => return ToolResult::error(format!("Cannot resolve working root: {}", e)),
            };
            let canonical_candidate = match candidate.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return ToolResult::error(format!("Cannot resolve directory '{}': {}", dir, e))
                }
            };
            if !canonical_candidate.starts_with(&canonical_root) {
                return ToolResult::error(format!(
                    "Directory '{}' is outside the allowed root '{}'",
                    dir,
                    canonical_root.display()
                ));
            }
            canonical_candidate
        } else {
            base_cwd
        };

        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);
        let command = format!("cargo {}", input.args);

        let requested = NetworkAccess::from_input(input.network.as_deref());
        let access = match ctx.network_policy {
            Some(ref policy) => policy.check(self.name(), &command, requested).await,
            None => requested,
        };

        let mut cmd = shell_command(&command, access);
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut content = String::new();
                if !stdout.is_empty() {
                    content.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&stderr);
                }

                if output.status.success() {
                    if content.is_empty() {
                        ToolResult::success("(cargo completed with no output)")
                    } else {
                        ToolResult::success(content)
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    ToolResult::error(format!("Exit code {}\n{}", code, content))
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Failed to execute cargo: {}", e)),
            Err(_) => ToolResult::error(format!("cargo timed out after {}ms", timeout_ms)),
        }
    }
}
