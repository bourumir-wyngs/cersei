//! Cargo tool: run cargo commands.

use super::*;
use serde::Deserialize;
use std::process::Stdio;

pub struct CargoTool;

#[async_trait]
impl Tool for CargoTool {
    fn name(&self) -> &str { "Cargo" }

    fn description(&self) -> &str {
        "Run a cargo command (e.g. build, test, run, check, clippy, fmt, publish). \
        The working directory persists between commands."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::Execute }
    fn category(&self) -> ToolCategory { ToolCategory::Shell }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "args": {
                    "type": "string",
                    "description": "Arguments to pass to cargo, e.g. \"build --release\", \"test\", \"clippy -- -D warnings\""
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (max 600000)"
                }
            },
            "required": ["args"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            args: String,
            timeout: Option<u64>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let shell_state = session_shell_state(&ctx.session_id);
        let cwd = {
            let state = shell_state.lock();
            state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone())
        };

        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);
        let command = format!("cargo {}", input.args);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &command])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            cmd.output(),
        )
        .await;

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
