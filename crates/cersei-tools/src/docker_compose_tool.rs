//! DockerCompose tool: run docker compose workflows and diagnostics from the workspace.

use super::*;
use crate::shell_sandbox::resolve_directory_in_workspace;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub struct DockerComposeTool;

const DEFAULT_TIMEOUT_MS: u64 = 900_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const DEFAULT_LOG_TAIL: usize = 100;
const DEFAULT_DIAGNOSTIC_LOG_TAIL: usize = 25;
const DEFAULT_COMPOSE_COMMAND: [&str; 2] = ["docker", "compose"];

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DockerComposeAction {
    Build {
        services: Option<Vec<String>>,
        pull: Option<bool>,
        no_cache: Option<bool>,
    },
    Up {
        services: Option<Vec<String>>,
        detach: Option<bool>,
        build: Option<bool>,
        force_recreate: Option<bool>,
        remove_orphans: Option<bool>,
    },
    Down {
        remove_orphans: Option<bool>,
        volumes: Option<bool>,
        images: Option<String>,
    },
    Ps {
        services: Option<Vec<String>>,
        all: Option<bool>,
    },
    Logs {
        services: Option<Vec<String>>,
        tail: Option<usize>,
        timestamps: Option<bool>,
    },
    Config {
        services_only: Option<bool>,
        images_only: Option<bool>,
    },
    Images {
        services: Option<Vec<String>>,
    },
    Diagnostics {
        services: Option<Vec<String>>,
        tail: Option<usize>,
        include_logs: Option<bool>,
    },
}

#[derive(Debug, Deserialize)]
pub struct DockerComposeInput {
    #[serde(flatten)]
    pub action: DockerComposeAction,
    pub compose_command: Option<Vec<String>>,
    pub workdir: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposeCommand {
    program: String,
    base_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl CommandOutput {
    fn is_success(&self) -> bool {
        self.exit_code == 0
    }

    fn combined_output(&self) -> String {
        let stdout = self.stdout.trim_end();
        let stderr = self.stderr.trim_end();

        match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => stdout.to_string(),
            (true, false) => stderr.to_string(),
            (false, false) => format!("{stdout}\n{stderr}"),
        }
    }
}

#[async_trait]
impl Tool for DockerComposeTool {
    fn name(&self) -> &str {
        "DockerCompose"
    }

    fn description(&self) -> &str {
        "Run docker compose workflows inside the workspace. Supports build, up, down, ps, logs, config, images, and bundled diagnostics. \
         `compose_command` defaults to [\"docker\", \"compose\"] and can be overridden for `docker-compose`, wrappers, or compose files. \
         `env` adds extra environment variables to the compose process. Use `workdir` to point at the compose project directory. \
         `up` defaults to detached mode unless `detach=false` is passed."
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
                "action": {
                    "type": "string",
                    "enum": ["build", "up", "down", "ps", "logs", "config", "images", "diagnostics"],
                    "description": "Compose action to perform."
                },
                "compose_command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Override the compose command as a program plus fixed arguments. Defaults to [\"docker\", \"compose\"]. Example: [\"docker\", \"compose\", \"-f\", \"docker-compose.dev.yml\"]."
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional subdirectory relative to the workspace root where the compose command should run."
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Optional extra environment variables passed to the compose process. Default: none."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds for the compose command. Defaults to 900000ms and is capped at 3600000ms."
                },
                "services": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of compose services to target for build, up, ps, logs, images, or diagnostics."
                },
                "pull": {
                    "type": "boolean",
                    "description": "Build action: attempt to pull a newer base image."
                },
                "no_cache": {
                    "type": "boolean",
                    "description": "Build action: disable the build cache."
                },
                "detach": {
                    "type": "boolean",
                    "description": "Up action: run in detached mode. Defaults to true."
                },
                "build": {
                    "type": "boolean",
                    "description": "Up action: build images before starting containers."
                },
                "force_recreate": {
                    "type": "boolean",
                    "description": "Up action: recreate containers even if configuration and image are unchanged."
                },
                "remove_orphans": {
                    "type": "boolean",
                    "description": "Up or down action: remove containers for services not defined in the current compose file."
                },
                "volumes": {
                    "type": "boolean",
                    "description": "Down action: remove named volumes declared in the compose file and anonymous volumes attached to containers."
                },
                "images": {
                    "type": "string",
                    "enum": ["all", "local"],
                    "description": "Down action: remove images used by services. `local` removes unnamed images, `all` removes all service images."
                },
                "all": {
                    "type": "boolean",
                    "description": "Ps action: include stopped containers."
                },
                "tail": {
                    "type": "integer",
                    "description": "Logs or diagnostics action: number of lines to show. Defaults to 100 for logs and 25 for diagnostics."
                },
                "timestamps": {
                    "type": "boolean",
                    "description": "Logs action: include timestamps."
                },
                "services_only": {
                    "type": "boolean",
                    "description": "Config action: output only the service names."
                },
                "images_only": {
                    "type": "boolean",
                    "description": "Config action: output only the referenced images."
                },
                "include_logs": {
                    "type": "boolean",
                    "description": "Diagnostics action: include recent logs. Defaults to true."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: DockerComposeInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let compose_command = match resolve_compose_command(input.compose_command.as_deref()) {
            Ok(command) => command,
            Err(err) => return ToolResult::error(err),
        };
        let workdir = match resolve_compose_workdir(input.workdir.as_deref(), ctx) {
            Ok(path) => path,
            Err(err) => return ToolResult::error(err),
        };
        let env = match validate_env(input.env.unwrap_or_default()) {
            Ok(env) => env,
            Err(err) => return ToolResult::error(err),
        };
        let timeout_ms = input
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        match build_action_args(&input.action) {
            Ok(Some(args)) => {
                match run_compose_command(&compose_command, &args, &workdir, &env, timeout_ms).await
                {
                    Ok(output) if output.is_success() => {
                        let content = output.combined_output();
                        if content.is_empty() {
                            ToolResult::success("(DockerCompose completed with no output)")
                        } else {
                            ToolResult::success(content)
                        }
                    }
                    Ok(output) => {
                        let body = output.combined_output();
                        if body.is_empty() {
                            ToolResult::error(format!(
                                "Exit code {} (DockerCompose produced no output)",
                                output.exit_code
                            ))
                        } else {
                            ToolResult::error(format!("Exit code {}\n{}", output.exit_code, body))
                        }
                    }
                    Err(err) => ToolResult::error(err),
                }
            }
            Ok(None) => {
                run_diagnostics(&compose_command, &workdir, &env, timeout_ms, &input.action).await
            }
            Err(err) => ToolResult::error(err),
        }
    }
}

fn resolve_compose_command(
    command: Option<&[String]>,
) -> std::result::Result<ComposeCommand, String> {
    let parts = match command {
        Some(parts) => {
            let normalized: Vec<String> = parts
                .iter()
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect();
            if normalized.is_empty() {
                return Err(
                    "`compose_command` must contain at least the executable name".to_string(),
                );
            }
            normalized
        }
        None => DEFAULT_COMPOSE_COMMAND
            .iter()
            .map(|part| (*part).to_string())
            .collect(),
    };

    Ok(ComposeCommand {
        program: parts[0].clone(),
        base_args: parts[1..].to_vec(),
    })
}

fn resolve_compose_workdir(
    workdir: Option<&str>,
    ctx: &ToolContext,
) -> std::result::Result<PathBuf, String> {
    let requested = workdir.map(str::trim).filter(|dir| !dir.is_empty());
    resolve_directory_in_workspace(
        &ctx.working_dir,
        requested,
        &ctx.working_dir,
        "docker compose",
    )
    .map(|(cwd, _)| cwd)
}

fn validate_env(
    env: HashMap<String, String>,
) -> std::result::Result<HashMap<String, String>, String> {
    for (key, value) in &env {
        if key.trim().is_empty() {
            return Err("Environment variable names must not be empty".to_string());
        }
        if key.contains('=') {
            return Err(format!(
                "Environment variable '{}' is invalid: names must not contain '='",
                key
            ));
        }
        if key.contains('\0') || value.contains('\0') {
            return Err(format!(
                "Environment variable '{}' is invalid: names and values must not contain NUL bytes",
                key
            ));
        }
    }

    Ok(env)
}

fn normalize_services(services: &Option<Vec<String>>) -> Vec<String> {
    services
        .as_ref()
        .into_iter()
        .flatten()
        .map(|service| service.trim().to_string())
        .filter(|service| !service.is_empty())
        .collect()
}

fn build_action_args(
    action: &DockerComposeAction,
) -> std::result::Result<Option<Vec<String>>, String> {
    match action {
        DockerComposeAction::Build {
            services,
            pull,
            no_cache,
        } => {
            let mut args = vec!["build".to_string()];
            if pull.unwrap_or(false) {
                args.push("--pull".to_string());
            }
            if no_cache.unwrap_or(false) {
                args.push("--no-cache".to_string());
            }
            args.extend(normalize_services(services));
            Ok(Some(args))
        }
        DockerComposeAction::Up {
            services,
            detach,
            build,
            force_recreate,
            remove_orphans,
        } => {
            let mut args = vec!["up".to_string()];
            if detach.unwrap_or(true) {
                args.push("--detach".to_string());
            }
            if build.unwrap_or(false) {
                args.push("--build".to_string());
            }
            if force_recreate.unwrap_or(false) {
                args.push("--force-recreate".to_string());
            }
            if remove_orphans.unwrap_or(false) {
                args.push("--remove-orphans".to_string());
            }
            args.extend(normalize_services(services));
            Ok(Some(args))
        }
        DockerComposeAction::Down {
            remove_orphans,
            volumes,
            images,
        } => {
            let mut args = vec!["down".to_string()];
            if remove_orphans.unwrap_or(false) {
                args.push("--remove-orphans".to_string());
            }
            if volumes.unwrap_or(false) {
                args.push("--volumes".to_string());
            }
            if let Some(images) = images
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                match images {
                    "all" | "local" => {
                        args.push("--rmi".to_string());
                        args.push(images.to_string());
                    }
                    other => {
                        return Err(format!(
                            "Invalid `images` value '{}'. Use 'all' or 'local'.",
                            other
                        ));
                    }
                }
            }
            Ok(Some(args))
        }
        DockerComposeAction::Ps { services, all } => {
            let mut args = vec!["ps".to_string()];
            if all.unwrap_or(false) {
                args.push("--all".to_string());
            }
            args.extend(normalize_services(services));
            Ok(Some(args))
        }
        DockerComposeAction::Logs {
            services,
            tail,
            timestamps,
        } => Ok(Some(logs_args(
            normalize_services(services),
            tail.unwrap_or(DEFAULT_LOG_TAIL),
            timestamps.unwrap_or(false),
        ))),
        DockerComposeAction::Config {
            services_only,
            images_only,
        } => {
            let services_only = services_only.unwrap_or(false);
            let images_only = images_only.unwrap_or(false);
            if services_only && images_only {
                return Err(
                    "`services_only` and `images_only` cannot both be true for action `config`"
                        .to_string(),
                );
            }

            let mut args = vec!["config".to_string()];
            if services_only {
                args.push("--services".to_string());
            } else if images_only {
                args.push("--images".to_string());
            }
            Ok(Some(args))
        }
        DockerComposeAction::Images { services } => {
            let mut args = vec!["images".to_string()];
            args.extend(normalize_services(services));
            Ok(Some(args))
        }
        DockerComposeAction::Diagnostics { .. } => Ok(None),
    }
}

fn logs_args(services: Vec<String>, tail: usize, timestamps: bool) -> Vec<String> {
    let mut args = vec![
        "logs".to_string(),
        "--tail".to_string(),
        tail.to_string(),
        "--no-color".to_string(),
    ];
    if timestamps {
        args.push("--timestamps".to_string());
    }
    args.extend(services);
    args
}

async fn run_diagnostics(
    compose_command: &ComposeCommand,
    workdir: &Path,
    env: &HashMap<String, String>,
    timeout_ms: u64,
    action: &DockerComposeAction,
) -> ToolResult {
    let DockerComposeAction::Diagnostics {
        services,
        tail,
        include_logs,
    } = action
    else {
        return ToolResult::error(
            "Internal error: diagnostics requested for non-diagnostics action",
        );
    };

    let services = normalize_services(services);
    let tail = tail.unwrap_or(DEFAULT_DIAGNOSTIC_LOG_TAIL);
    let include_logs = include_logs.unwrap_or(true);

    let mut commands = vec![
        vec!["config".to_string(), "--services".to_string()],
        vec!["ps".to_string(), "--all".to_string()],
        vec!["images".to_string()],
    ];

    if include_logs {
        commands.push(logs_args(services, tail, false));
    }

    let mut sections = Vec::new();
    let mut had_success = false;

    for args in commands {
        let invocation = render_invocation(compose_command, &args);
        match run_compose_command(compose_command, &args, workdir, env, timeout_ms).await {
            Ok(output) => {
                let body = output.combined_output();
                let rendered = if body.is_empty() {
                    "(no output)".to_string()
                } else {
                    body
                };
                if output.is_success() {
                    had_success = true;
                    sections.push(format!("$ {invocation}\n{rendered}"));
                } else {
                    sections.push(format!(
                        "$ {invocation}\nExit code {}\n{}",
                        output.exit_code, rendered
                    ));
                }
            }
            Err(err) => {
                sections.push(format!("$ {invocation}\n{err}"));
            }
        }
    }

    if had_success {
        ToolResult::success(sections.join("\n\n"))
    } else {
        ToolResult::error(sections.join("\n\n"))
    }
}

async fn run_compose_command(
    compose_command: &ComposeCommand,
    args: &[String],
    workdir: &Path,
    env: &HashMap<String, String>,
    timeout_ms: u64,
) -> std::result::Result<CommandOutput, String> {
    let mut cmd = tokio::process::Command::new(&compose_command.program);
    cmd.args(&compose_command.base_args)
        .args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for (key, value) in env {
        cmd.env(key, value);
    }

    let output = match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        cmd.output(),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            return Err(format!(
                "Failed to execute '{}': {}",
                render_invocation(compose_command, args),
                err
            ))
        }
        Err(_) => {
            return Err(format!(
                "Command timed out after {}ms: {}",
                timeout_ms,
                render_invocation(compose_command, args)
            ))
        }
    };

    Ok(CommandOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn render_invocation(compose_command: &ComposeCommand, args: &[String]) -> String {
    std::iter::once(compose_command.program.as_str())
        .chain(compose_command.base_args.iter().map(String::as_str))
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }

    if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':'))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_docker_compose_command() {
        let command = resolve_compose_command(None).unwrap();
        assert_eq!(
            command,
            ComposeCommand {
                program: "docker".to_string(),
                base_args: vec!["compose".to_string()],
            }
        );
    }

    #[test]
    fn rejects_empty_custom_compose_command() {
        let err = resolve_compose_command(Some(&[])).unwrap_err();
        assert!(err.contains("`compose_command`"));
    }

    #[test]
    fn up_defaults_to_detached_mode() {
        let args = build_action_args(&DockerComposeAction::Up {
            services: Some(vec!["web".to_string()]),
            detach: None,
            build: Some(true),
            force_recreate: Some(true),
            remove_orphans: Some(true),
        })
        .unwrap()
        .unwrap();

        assert_eq!(
            args,
            vec![
                "up",
                "--detach",
                "--build",
                "--force-recreate",
                "--remove-orphans",
                "web"
            ]
        );
    }

    #[test]
    fn down_supports_rmi_and_volumes() {
        let args = build_action_args(&DockerComposeAction::Down {
            remove_orphans: Some(true),
            volumes: Some(true),
            images: Some("local".to_string()),
        })
        .unwrap()
        .unwrap();

        assert_eq!(
            args,
            vec!["down", "--remove-orphans", "--volumes", "--rmi", "local"]
        );
    }

    #[test]
    fn diagnostics_uses_recent_logs_by_default() {
        let args = logs_args(vec!["api".to_string()], DEFAULT_DIAGNOSTIC_LOG_TAIL, false);
        assert_eq!(args, vec!["logs", "--tail", "25", "--no-color", "api"]);
    }

    #[test]
    fn env_rejects_empty_keys() {
        let err = validate_env(HashMap::from([(String::new(), "value".to_string())])).unwrap_err();
        assert!(err.contains("must not be empty"));
    }
}
