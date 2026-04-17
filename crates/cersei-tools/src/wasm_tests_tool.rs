//! wasm_tests tool: run wasm32-wasip1-compatible Rust tests in a tightly sandboxed flow.

use super::*;
use crate::network_policy::{
    firejailed_shell_command_with_extra_firejail_args, NetworkAccess, NetworkDecision,
};
use crate::shell_sandbox::{
    home_entries_and_workspace_firejail_args, read_only_workspace_firejail_args,
    resolve_directory_in_workspace,
};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use toml_edit::{DocumentMut, Item};

pub struct WasmTestsTool;

const CONFIGURE_PROMPT: &str = "Configure this project to run wasm32-wasip1 tests?";
const RUN_PROMPT: &str = "Run wasm_tests (no fs after building and no network, fully sandboxed). Separate run in two parts: build the test binary (sandbox restrictions like for Cargo tool) and run the test binary (combine wasm32-wasip1 runner with firejail to suppress all network and all filesystem except the binary itself)";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    project_root: Option<String>,
    test_name: Option<String>,
    artifact: Option<String>,
    args: Option<Vec<String>>,
    timeout: Option<u64>,
}

fn project_has_wasm_test_config(project_root: &Path) -> bool {
    let config_path = project_root.join(".cargo/config.toml");
    let Ok(contents) = fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = contents.parse::<DocumentMut>() else {
        return false;
    };

    doc.get("target")
        .and_then(Item::as_table_like)
        .and_then(|target| target.get("wasm32-wasip1"))
        .and_then(Item::as_table_like)
        .and_then(|target| target.get("runner"))
        .is_some()
}

fn ensure_project_root(path: &Path) -> std::result::Result<(), String> {
    let manifest = path.join("Cargo.toml");
    if manifest.is_file() {
        Ok(())
    } else {
        Err(format!("No Cargo.toml found in '{}'", path.display()))
    }
}

fn wasm_test_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".cargo/config.toml")
}

fn approval_result(prompt: &str, metadata_kind: &str, project_root: &Path) -> ToolResult {
    ToolResult::success(prompt).with_metadata(serde_json::json!({
        "approval": "required",
        "kind": metadata_kind,
        "project_root": project_root,
        "prompt": prompt,
    }))
}

fn shell_single_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

fn resolve_artifact_path(
    project_root: &Path,
    artifact: Option<&str>,
) -> std::result::Result<Option<PathBuf>, ToolResult> {
    let Some(artifact) = artifact.map(str::trim).filter(|artifact| !artifact.is_empty()) else {
        return Ok(None);
    };

    let candidate = project_root.join(artifact);
    if !candidate.exists() {
        return Err(ToolResult::error(format!(
            "Artifact '{}' does not exist under '{}'",
            artifact,
            project_root.display()
        )));
    }

    let canonical_project_root = project_root.canonicalize().map_err(|e| {
        ToolResult::error(format!(
            "Cannot resolve project root '{}': {}",
            project_root.display(),
            e
        ))
    })?;
    let canonical_artifact = candidate.canonicalize().map_err(|e| {
        ToolResult::error(format!(
            "Cannot resolve artifact '{}': {}",
            candidate.display(),
            e
        ))
    })?;

    if !canonical_artifact.starts_with(&canonical_project_root) {
        return Err(ToolResult::error(format!(
            "Artifact '{}' resolves outside project root '{}'",
            artifact,
            project_root.display()
        )));
    }

    if !canonical_artifact.is_file() {
        return Err(ToolResult::error(format!(
            "Artifact '{}' is not a file",
            canonical_artifact.display()
        )));
    }

    Ok(Some(canonical_artifact))
}

fn build_command(test_name: Option<&str>) -> String {
    match test_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(test_name) => format!("cargo test --target wasm32-wasip1 {} --no-run", test_name),
        None => "cargo test --target wasm32-wasip1 --no-run".to_string(),
    }
}

fn run_command(artifact: &Path, test_name: Option<&str>, args: &[String]) -> String {
    let mut command = shell_single_quote(&artifact.display().to_string());

    if let Some(test_name) = test_name.map(str::trim).filter(|name| !name.is_empty()) {
        command.push(' ');
        command.push_str(&shell_single_quote(test_name));
    }

    for arg in args {
        command.push(' ');
        command.push_str(&shell_single_quote(arg));
    }

    command
}

async fn execute_shell_command(
    command: &str,
    current_dir: &Path,
    network_access: NetworkAccess,
    firejail_args: &[std::ffi::OsString],
    timeout_ms: u64,
) -> ToolResult {
    let mut cmd = firejailed_shell_command_with_extra_firejail_args(
        command,
        network_access,
        firejail_args,
    );
    cmd.current_dir(current_dir)
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
                    ToolResult::success("(command completed with no output)")
                } else {
                    ToolResult::success(content)
                }
            } else {
                let code = output.status.code().unwrap_or(-1);
                ToolResult::error(format!("Exit code {}\n{}", code, content))
            }
        }
        Ok(Err(e)) => ToolResult::error(format!("Failed to execute command: {}", e)),
        Err(_) => ToolResult::error(format!("command timed out after {}ms", timeout_ms)),
    }
}

#[async_trait]
impl Tool for WasmTestsTool {
    fn name(&self) -> &str {
        "wasm_tests"
    }

    fn description(&self) -> &str {
        "Can safely run Rust tests that are wasm32-wasip1 compatible (no filesystem, no network)."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Testing
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project_root": {
                    "type": "string",
                    "description": "Optional project root relative to the workspace root. Defaults to the current tool working directory."
                },
                "test_name": {
                    "type": "string",
                    "description": "Optional Rust test name to run inside wasm test artifacts."
                },
                "artifact": {
                    "type": "string",
                    "description": "Optional wasm artifact path relative to the chosen project root."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional additional arguments for the wasm test binary."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (reserved for future execution implementation)."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let shell_state = session_shell_state(&ctx.session_id);
        let base_cwd = {
            let state = shell_state.lock();
            state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone())
        };

        let (project_root, workspace_root) = match resolve_directory_in_workspace(
            &base_cwd,
            input
                .project_root
                .as_deref()
                .map(str::trim)
                .filter(|dir| !dir.is_empty()),
            &ctx.working_dir,
            self.name(),
        ) {
            Ok(paths) => paths,
            Err(err) => return ToolResult::error(err),
        };

        if let Err(err) = ensure_project_root(&project_root) {
            return ToolResult::error(err);
        }

        if !project_has_wasm_test_config(&project_root) {
            return approval_result(CONFIGURE_PROMPT, "configure", &project_root).with_metadata(
                serde_json::json!({
                    "approval": "required",
                    "kind": "configure",
                    "project_root": project_root,
                    "config_path": wasm_test_config_path(&project_root),
                    "prompt": CONFIGURE_PROMPT,
                }),
            );
        }

        if let Some(ref policy) = ctx.network_policy {
            match policy
                .check(self.name(), RUN_PROMPT, NetworkAccess::Blocked)
                .await
            {
                NetworkDecision::Allow(_) => {}
                NetworkDecision::Deny(reason) => {
                    return ToolResult::error(format!("Permission denied: {}", reason));
                }
            }
        }

        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);
        let args = input.args.unwrap_or_default();
        let test_name = input.test_name.as_deref();

        let explicit_artifact = match resolve_artifact_path(&project_root, input.artifact.as_deref()) {
            Ok(path) => path,
            Err(err) => return err,
        };

        let build_output = if let Some(artifact) = explicit_artifact.as_ref() {
            ToolResult::success(format!(
                "Skipping build because explicit artifact was provided: {}",
                artifact.display()
            ))
        } else {
            let build_firejail_args =
                home_entries_and_workspace_firejail_args(&workspace_root, &[".cargo", ".rustup"]);
            let build_command = build_command(test_name);
            let result = execute_shell_command(
                &build_command,
                &project_root,
                NetworkAccess::Blocked,
                &build_firejail_args,
                timeout_ms,
            )
            .await;

            if result.is_error {
                return result;
            }
            result
        };

        let config = wasm_tests::WasmTestConfig::new(&project_root);
        let artifact_path = match explicit_artifact {
            Some(path) => path,
            None => match wasm_tests::discover_artifacts(&config) {
                Ok(mut artifacts) => match artifacts.len() {
                    0 => {
                        return ToolResult::error(
                            "No wasm test artifacts were discovered after building".to_string(),
                        )
                    }
                    1 => artifacts.remove(0).path,
                    count => {
                        let listed = artifacts
                            .into_iter()
                            .map(|artifact| artifact.path.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n");
                        return ToolResult::error(format!(
                            "Discovered {} wasm test artifacts; please specify 'artifact'.\n{}",
                            count, listed
                        ));
                    }
                },
                Err(err) => {
                    return ToolResult::error(format!(
                        "Failed to discover wasm test artifacts: {}",
                        err
                    ))
                }
            },
        };

        let artifact_relative = match artifact_path.strip_prefix(&workspace_root) {
            Ok(path) => path,
            Err(_) => {
                return ToolResult::error(format!(
                    "Artifact '{}' is outside workspace root '{}'",
                    artifact_path.display(),
                    workspace_root.display()
                ))
            }
        };
        let artifact_relative_owned = artifact_relative.to_string_lossy().to_string();
        let writable_entries = vec![artifact_relative_owned.as_str()];
        let run_firejail_args = read_only_workspace_firejail_args(&workspace_root, &writable_entries);
        let run_command = run_command(&artifact_path, test_name, &args);
        let run_output = execute_shell_command(
            &run_command,
            artifact_path.parent().unwrap_or(project_root.as_path()),
            NetworkAccess::Blocked,
            &run_firejail_args,
            timeout_ms,
        )
        .await;

        if run_output.is_error {
            return run_output;
        }

        let build_text = build_output.content.trim();
        let run_text = run_output.content.trim();
        let mut content = String::new();
        content.push_str("Build:\n");
        content.push_str(if build_text.is_empty() {
            "(build completed with no output)"
        } else {
            build_text
        });
        content.push_str("\n\nRun:\n");
        content.push_str(if run_text.is_empty() {
            "(run completed with no output)"
        } else {
            run_text
        });

        ToolResult::success(content).with_metadata(serde_json::json!({
            "artifact": artifact_path,
            "project_root": project_root,
            "sandbox": {
                "network": "blocked",
                "build_filesystem": "workspace plus cargo/rustup home entries",
                "run_filesystem": "workspace read-only with artifact writable"
            }
        }))
    }
}
