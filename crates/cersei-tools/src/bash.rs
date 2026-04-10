//! Bash tool: execute shell commands.

use super::*;
use crate::network_policy::{shell_command, NetworkAccess};
use serde::Deserialize;
use std::process::Stdio;

pub struct BashTool;

fn tool_override_for_command(command: &str) -> Option<(&str, &'static str)> {
    let cmd_trim = command.trim();
    let cmd_base_full = cmd_trim.split_whitespace().next().unwrap_or("");
    let cmd_base = cmd_base_full.rsplit('/').next().unwrap_or(cmd_base_full);
    let tool_name = match cmd_base {
        "ls" | "tree" | "exa" | "lsd" => Some("ListDirectory"),
        "grep" | "rg" | "ag" => Some("Grep"),
        "cat" | "bat" | "head" | "tail" | "less" | "more" => Some("Read"),
        "sed" => Some("Sed"),
        "find" | "fd" => Some("Glob"),
        "npm" | "yarn" | "pnpm" => Some("Npm"),
        "npx" => Some("Npx"),
        "cargo" => Some("Cargo"),
        "git" => Some("Git"),
        "mysql" => Some("MySql"),
        "psql" => Some("PostgreSql"),
        "curl" | "wget" => Some("WebFetch"),
        "pwsh" => Some("PowerShell"),
        _ => None,
    }?;
    Some((cmd_base, tool_name))
}

fn tool_override_error(command: &str) -> Option<String> {
    let (cmd_base, tool_name) = tool_override_for_command(command)?;
    Some(format!(
        "Action denied, do not use bash for '{}', use {}. If does not do what you want or is buggy, report to the user.",
        cmd_base, tool_name
    ))
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return its output. The working directory persists \
        between commands. Do not use this tool for cat, grep, ls, git and other actions \
        the like special tools exist."
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
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (max 600000)"
                },
                "network": {
                    "type": "string",
                    "enum": ["local", "full"],
                    "description": "Network access required. Omit to request normal network access. Use 'local' for local network only (e.g. localhost services). Use 'full' when the command needs external network access."
                }
            },
            "required": ["command"]
        })
    }

    fn preflight(&self, input: &Value, _ctx: &ToolContext) -> Option<ToolResult> {
        let command = input.get("command")?.as_str()?;
        tool_override_error(command).map(ToolResult::error)
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            command: String,
            timeout: Option<u64>,
            network: Option<String>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if let Some(message) = tool_override_error(&input.command) {
            return ToolResult::error(message);
        }

        let shell_state = session_shell_state(&ctx.session_id);
        let (cwd, env_vars) = {
            let state = shell_state.lock();
            (
                state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone()),
                state.env_vars.clone(),
            )
        };

        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);

        let requested = NetworkAccess::from_input(input.network.as_deref());
        let access = match ctx.network_policy {
            Some(ref policy) => policy.check(self.name(), &input.command, requested).await,
            None => requested,
        };

        let mut cmd = shell_command(&input.command, access);
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (k, v) in &env_vars {
            cmd.env(k, v);
        }

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                // Update shell state for cd commands
                if input.command.trim().starts_with("cd ") {
                    let dir = input.command.trim().strip_prefix("cd ").unwrap().trim();
                    let new_cwd = if dir.starts_with('/') {
                        PathBuf::from(dir)
                    } else {
                        cwd.join(dir)
                    };
                    if new_cwd.exists() {
                        shell_state.lock().cwd = Some(new_cwd);
                    }
                }

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
                        ToolResult::success("(Bash completed with no output)")
                    } else {
                        ToolResult::success(content)
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    ToolResult::error(format!("Exit code {}\n{}", code, content))
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Failed to execute: {}", e)),
            Err(_) => ToolResult::error(format!("Command timed out after {}ms", timeout_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preflight_rejects_ls_before_execution() {
        let tool = BashTool;
        let result = tool.preflight(&json!({"command": "ls -la"}), &ToolContext::default());

        let result = result.expect("expected preflight rejection");
        assert!(result.is_error);
        assert!(result.content.contains("use ListDirectory"));
    }

    #[test]
    fn preflight_allows_normal_shell_commands() {
        let tool = BashTool;
        let result = tool.preflight(&json!({"command": "echo hello"}), &ToolContext::default());

        assert!(result.is_none());
    }
}
