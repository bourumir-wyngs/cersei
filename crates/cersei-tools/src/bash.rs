//! Bash tool: execute shell commands.

use super::*;
use crate::network_policy::{shell_command, NetworkAccess, NetworkDecision};
use crate::shell_sandbox::resolve_directory_in_workspace;
use serde::Deserialize;
use std::process::Stdio;

pub struct BashTool;

fn tool_override_for_command(command: &str) -> Option<(&str, &'static str)> {
    let cmd_trim = command.trim();
    let cmd_base_full = cmd_trim.split_whitespace().next().unwrap_or("");
    let cmd_base = cmd_base_full.rsplit('/').next().unwrap_or(cmd_base_full);
    if crate::web_tests_tool::is_supported_web_test_command(cmd_trim) {
        return Some((cmd_base, "web_tests"));
    }
    let tool_name = match cmd_base {
        "ls" | "tree" | "exa" | "lsd" => Some("ListDirectory"),
        "grep" | "rg" | "ag" => Some("Grep"),
        "cat" | "bat" | "head" | "tail" | "less" | "more" => Some("Read"),
        "sed" => Some("Sed"),
        "find" | "fd" => Some("Glob"),
        "npm" | "yarn" | "pnpm" => Some("Npm"),
        "npx" => Some("Npx"),
        "cargo" => Some("Cargo"),
        "pytest" => Some("Pytest"),
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

fn cd_command_error(command: &str) -> Option<&'static str> {
    (command.trim().split_whitespace().next() == Some("cd"))
        .then_some("Do not use cd, use workdir parameter")
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return its output. The working directory defaults \
        to the workspace root, persists between commands, and can be overridden with \
        workdir. Do not use this tool for cat, grep, ls, git and other actions the \
        like special tools exist."
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
                "workdir": {
                    "type": "string",
                    "description": "Optional subdirectory relative to the current Bash directory. If omitted or empty, reuse the session directory. Must not escape the workspace root."
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
        if let Some(message) = cd_command_error(command) {
            return Some(ToolResult::error(message));
        }
        tool_override_error(command).map(ToolResult::error)
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            command: String,
            workdir: Option<String>,
            timeout: Option<u64>,
            network: Option<String>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if let Some(message) = cd_command_error(&input.command) {
            return ToolResult::error(message);
        }

        if let Some(message) = tool_override_error(&input.command) {
            return ToolResult::error(message);
        }

        let shell_state = session_shell_state(&ctx.session_id);
        let (base_cwd, env_vars) = {
            let state = shell_state.lock();
            (
                state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone()),
                state.env_vars.clone(),
            )
        };
        let requested_workdir = input
            .workdir
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty());
        let (cwd, _) = match resolve_directory_in_workspace(
            &base_cwd,
            requested_workdir,
            &ctx.working_dir,
            "bash",
        ) {
            Ok(paths) => paths,
            Err(err) => return ToolResult::error(err),
        };

        if requested_workdir.is_some() {
            shell_state.lock().cwd = Some(cwd.clone());
        }

        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);

        let requested = NetworkAccess::from_input(input.network.as_deref());
        let access = match ctx.network_policy {
            Some(ref policy) => match policy.check(self.name(), &input.command, requested).await {
                NetworkDecision::Allow(access) => access,
                NetworkDecision::Deny(reason) => {
                    return ToolResult::error(format!("Permission denied: {}", reason));
                }
            },
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
    use std::path::PathBuf;
    use std::sync::Arc;

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

    #[test]
    fn preflight_rejects_pytest_before_execution() {
        let tool = BashTool;
        let result = tool.preflight(&json!({"command": "pytest -q"}), &ToolContext::default());

        let result = result.expect("expected preflight rejection");
        assert!(result.is_error);
        assert!(result.content.contains("use Pytest"));
    }

    #[test]
    fn preflight_rejects_web_tests_commands_before_execution() {
        let tool = BashTool;
        let result = tool.preflight(
            &json!({"command": "npm run test:web"}),
            &ToolContext::default(),
        );

        let result = result.expect("expected preflight rejection");
        assert!(result.is_error);
        assert!(result.content.contains("use web_tests"));
    }

    #[test]
    fn preflight_rejects_cd_and_suggests_workdir() {
        let tool = BashTool;
        let result = tool.preflight(
            &json!({"command": "cd src && pwd"}),
            &ToolContext::default(),
        );

        let result = result.expect("expected preflight rejection");
        assert!(result.is_error);
        assert_eq!(result.content, "Do not use cd, use workdir parameter");
    }

    fn test_ctx(session_id: String, working_dir: PathBuf) -> ToolContext {
        ToolContext {
            session_id,
            working_dir,
            permissions: Arc::new(permissions::AllowAll),
            ..ToolContext::default()
        }
    }

    #[tokio::test]
    async fn execute_uses_workdir_and_reuses_it_when_omitted() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("nested");
        std::fs::create_dir_all(&nested).expect("nested dir");

        let ctx = test_ctx(
            format!("bash-test-{}", uuid::Uuid::new_v4()),
            workspace.path().to_path_buf(),
        );
        let tool = BashTool;

        let first = tool
            .execute(
                json!({
                    "command": "pwd",
                    "workdir": "nested"
                }),
                &ctx,
            )
            .await;
        assert!(!first.is_error, "{}", first.content);
        assert_eq!(first.content.trim(), nested.display().to_string());

        let second = tool
            .execute(json!({"command": "pwd", "workdir": ""}), &ctx)
            .await;
        assert!(!second.is_error, "{}", second.content);
        assert_eq!(second.content.trim(), nested.display().to_string());
    }

    #[tokio::test]
    async fn execute_rejects_cd_commands() {
        let workspace = tempfile::tempdir().expect("workspace");
        let ctx = test_ctx(
            format!("bash-test-{}", uuid::Uuid::new_v4()),
            workspace.path().to_path_buf(),
        );
        let tool = BashTool;

        let result = tool.execute(json!({"command": "cd nested"}), &ctx).await;
        assert!(result.is_error);
        assert_eq!(result.content, "Do not use cd, use workdir parameter");
    }
}
