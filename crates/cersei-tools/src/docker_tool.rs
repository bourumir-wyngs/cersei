//! DockerAssistant tool: AI-friendly observability and diagnostics tool for Docker environments.

use super::*;
use bollard::container::{InspectContainerOptions, ListContainersOptions, LogOutput, LogsOptions};
use bollard::image::ListImagesOptions;
use bollard::network::ListNetworksOptions;
use bollard::volume::ListVolumesOptions;
use bollard::Docker;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

pub struct DockerAssistantTool;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DockerAction {
    /// Returns a list of containers.
    GetContainers {
        /// If true, includes stopped containers.
        #[serde(default)]
        all: bool,
    },
    /// Inspects a container's details (environment variables are redacted for safety).
    InspectContainer { container_id: String },
    /// Retrieves recent logs from a container.
    GetLogs {
        container_id: String,
        /// Number of recent lines to fetch (default: 100).
        tail: Option<usize>,
    },
    /// Lists all images on the host.
    GetImages,
    /// Lists all docker networks.
    GetNetworks,
    /// Lists all docker volumes.
    GetVolumes,
}

#[derive(Deserialize)]
pub struct DockerAssistantInput {
    #[serde(flatten)]
    pub action: DockerAction,
}

#[async_trait]
impl Tool for DockerAssistantTool {
    fn name(&self) -> &str {
        "DockerAssistant"
    }

    fn description(&self) -> &str {
        "Read-first observability and diagnostics tool for Docker environments. \
         Provides structured access to container state, logs, networks, volumes, \
         and allows safe restarts. Automatically redacts secrets from environment variables."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform: get_containers, inspect_container, get_logs, get_images, get_networks, get_volumes"
                },
                "container_id": {
                    "type": "string",
                    "description": "Container ID or name. Required for inspect_container and get_logs."
                },
                "all": {
                    "type": "boolean",
                    "description": "Include stopped containers. Used with get_containers."
                },
                "tail": {
                    "type": "integer",
                    "description": "Number of recent log lines to fetch (default 100). Used with get_logs."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let input: DockerAssistantInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let docker = match Docker::connect_with_local_defaults() {
            Ok(d) => d,
            Err(e) => {
                return ToolResult::error(format!("Failed to connect to Docker daemon: {}", e))
            }
        };

        match input.action {
            DockerAction::GetContainers { all } => {
                let options = ListContainersOptions::<String> {
                    all,
                    ..Default::default()
                };
                match docker.list_containers(Some(options)).await {
                    Ok(containers) => {
                        // Simplify output
                        let simplified: Vec<_> = containers
                            .into_iter()
                            .map(|c| {
                                serde_json::json!({
                                    "id": c.id.unwrap_or_default(),
                                    "names": c.names.unwrap_or_default(),
                                    "image": c.image.unwrap_or_default(),
                                    "state": c.state.unwrap_or_default(),
                                    "status": c.status.unwrap_or_default(),
                                    "ports": c.ports.unwrap_or_default(),
                                })
                            })
                            .collect();
                        ToolResult::success(serde_json::to_string_pretty(&simplified).unwrap())
                    }
                    Err(e) => ToolResult::error(format!("Failed to list containers: {}", e)),
                }
            }
            DockerAction::InspectContainer { container_id } => {
                match docker
                    .inspect_container(&container_id, None::<InspectContainerOptions>)
                    .await
                {
                    Ok(mut info) => {
                        // Redact environment variables for safety
                        if let Some(config) = info.config.as_mut() {
                            if let Some(env) = config.env.as_mut() {
                                for e in env.iter_mut() {
                                    if let Some(idx) = e.find('=') {
                                        let key = &e[..idx];
                                        *e = format!("{}={}", key, "[REDACTED]");
                                    }
                                }
                            }
                        }
                        ToolResult::success(serde_json::to_string_pretty(&info).unwrap())
                    }
                    Err(e) => ToolResult::error(format!("Failed to inspect container: {}", e)),
                }
            }
            DockerAction::GetLogs { container_id, tail } => {
                let tail_str = tail.unwrap_or(100).to_string();
                let options = LogsOptions::<String> {
                    stdout: true,
                    stderr: true,
                    tail: tail_str,
                    ..Default::default()
                };
                let mut stream = docker.logs(&container_id, Some(options));
                let mut logs = String::new();
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(LogOutput::StdOut { message }) => {
                            logs.push_str(&String::from_utf8_lossy(&message));
                        }
                        Ok(LogOutput::StdErr { message }) => {
                            logs.push_str(&String::from_utf8_lossy(&message));
                        }
                        Ok(_) => {}
                        Err(e) => return ToolResult::error(format!("Error streaming logs: {}", e)),
                    }
                }
                ToolResult::success(logs)
            }
            DockerAction::GetImages => {
                match docker.list_images(None::<ListImagesOptions<String>>).await {
                    Ok(images) => {
                        let simplified: Vec<_> = images
                            .into_iter()
                            .map(|img| {
                                serde_json::json!({
                                    "id": img.id,
                                    "repo_tags": img.repo_tags,
                                    "size": img.size,
                                    "created": img.created,
                                })
                            })
                            .collect();
                        ToolResult::success(serde_json::to_string_pretty(&simplified).unwrap())
                    }
                    Err(e) => ToolResult::error(format!("Failed to list images: {}", e)),
                }
            }
            DockerAction::GetNetworks => {
                match docker
                    .list_networks(None::<ListNetworksOptions<String>>)
                    .await
                {
                    Ok(networks) => {
                        ToolResult::success(serde_json::to_string_pretty(&networks).unwrap())
                    }
                    Err(e) => ToolResult::error(format!("Failed to list networks: {}", e)),
                }
            }
            DockerAction::GetVolumes => {
                match docker
                    .list_volumes(None::<ListVolumesOptions<String>>)
                    .await
                {
                    Ok(volumes) => {
                        ToolResult::success(serde_json::to_string_pretty(&volumes).unwrap())
                    }
                    Err(e) => ToolResult::error(format!("Failed to list volumes: {}", e)),
                }
            }
        }
    }
}
