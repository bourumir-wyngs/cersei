//! DockerExec tool: perform modifying or exec actions on Docker containers.

use super::*;
use bollard::container::{RestartContainerOptions, StartContainerOptions, StopContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::Docker;
use futures::StreamExt;
use serde::Deserialize;

pub struct DockerExecTool;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DockerExecAction {
    /// Execute a command in a running container
    Exec {
        container_id: String,
        cmd: Vec<String>,
        /// Optional working directory
        workdir: Option<String>,
    },
    /// Restart a specific container
    RestartContainer { container_id: String },
    /// Stop a running container
    StopContainer { container_id: String },
    /// Start a stopped container
    StartContainer { container_id: String },
}

#[derive(Deserialize)]
pub struct DockerExecInput {
    #[serde(flatten)]
    pub action: DockerExecAction,
}

#[async_trait]
impl Tool for DockerExecTool {
    fn name(&self) -> &str {
        "DockerExec"
    }

    fn description(&self) -> &str {
        "Perform mutating actions on Docker containers (exec, restart, start, stop). \
         Use DockerAssistant for read-only observability instead."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform: exec, restart_container, stop_container, start_container"
                },
                "container_id": {
                    "type": "string",
                    "description": "Container ID or name. Required for all actions."
                },
                "cmd": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command to run inside the container. Required for exec."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory inside the container. Optional, used with exec."
                }
            },
            "required": ["action", "container_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let input: DockerExecInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let docker = match Docker::connect_with_local_defaults() {
            Ok(d) => d,
            Err(e) => return ToolResult::error(format!("Failed to connect to Docker daemon: {}", e)),
        };

        match input.action {
            DockerExecAction::Exec { container_id, cmd, workdir } => {
                let exec_options = CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    working_dir: workdir,
                    ..Default::default()
                };

                let exec = match docker.create_exec(&container_id, exec_options).await {
                    Ok(e) => e,
                    Err(e) => return ToolResult::error(format!("Failed to create exec: {}", e)),
                };

                match docker.start_exec(&exec.id, None::<StartExecOptions>).await {
                    Ok(StartExecResults::Attached { mut output, .. }) => {
                        let mut out = String::new();
                        while let Some(msg_result) = output.next().await {
                            match msg_result {
                                Ok(msg) => out.push_str(&String::from_utf8_lossy(&msg.into_bytes())),
                                Err(e) => return ToolResult::error(format!("Error reading exec output: {}", e)),
                            }
                        }
                        ToolResult::success(out)
                    }
                    Ok(StartExecResults::Detached) => ToolResult::success("Exec started detached.".to_string()),
                    Err(e) => ToolResult::error(format!("Failed to start exec: {}", e)),
                }
            }
            DockerExecAction::RestartContainer { container_id } => {
                match docker.restart_container(&container_id, None::<RestartContainerOptions>).await {
                    Ok(_) => ToolResult::success(format!("Successfully restarted container {}", container_id)),
                    Err(e) => ToolResult::error(format!("Failed to restart container: {}", e)),
                }
            }
            DockerExecAction::StopContainer { container_id } => {
                match docker.stop_container(&container_id, None::<StopContainerOptions>).await {
                    Ok(_) => ToolResult::success(format!("Successfully stopped container {}", container_id)),
                    Err(e) => ToolResult::error(format!("Failed to stop container: {}", e)),
                }
            }
            DockerExecAction::StartContainer { container_id } => {
                match docker.start_container(&container_id, None::<StartContainerOptions<String>>).await {
                    Ok(_) => ToolResult::success(format!("Successfully started container {}", container_id)),
                    Err(e) => ToolResult::error(format!("Failed to start container: {}", e)),
                }
            }
        }
    }
}
