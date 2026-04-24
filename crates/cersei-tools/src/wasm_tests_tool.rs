//! Wasm_tests tool: run wasm32-wasip1-compatible Rust tests in a tightly sandboxed flow.

use super::*;
use crate::network_policy::{
    firejailed_shell_command_with_extra_firejail_args, NetworkAccess, NetworkDecision,
};
use crate::permissions::{PermissionDecision, PermissionRequest};
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

const BUILD_PROMPT: &str = "Build Wasm_tests artifacts (Cargo-like sandbox with network allowed during build only). This permission is remembered for all future Wasm_tests builds in the session.";

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

fn configure_project(project_root: &Path) -> std::result::Result<PathBuf, String> {
    let cargo_dir = project_root.join(".cargo");
    fs::create_dir_all(&cargo_dir)
        .map_err(|e| format!("Failed to create '{}': {}", cargo_dir.display(), e))?;
    let config_path = cargo_dir.join("config.toml");
    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    let mut doc = existing.parse::<DocumentMut>().unwrap_or_default();
    doc["target"]["wasm32-wasip1"]["runner"] = toml_edit::value("wasm_test");
    fs::write(&config_path, doc.to_string())
        .map_err(|e| format!("Failed to write '{}': {}", config_path.display(), e))?;
    Ok(config_path)
}

fn shell_single_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

async fn ensure_configure_permission(
    project_root: &Path,
    ctx: &ToolContext,
) -> std::result::Result<(), ToolResult> {
    let config_path = wasm_test_config_path(project_root);
    let command = format!(
        "configure wasm32-wasip1 runner in {}",
        config_path.display()
    );
    let request = PermissionRequest {
        tool_name: "Wasm_tests".into(),
        tool_input: serde_json::json!({
            "command": command,
            "project_root": project_root.display().to_string(),
            "config_path": config_path.display().to_string(),
        }),
        permission_level: PermissionLevel::Write,
        description: format!("Configure Wasm_tests runner at {}", config_path.display()),
        id: format!("wasm-tests-configure-{}", uuid::Uuid::new_v4()),
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

fn resolve_artifact_path(
    project_root: &Path,
    workspace_root: &Path,
    artifact: Option<&str>,
) -> std::result::Result<Option<PathBuf>, ToolResult> {
    let Some(artifact) = artifact
        .map(str::trim)
        .filter(|artifact| !artifact.is_empty())
    else {
        return Ok(None);
    };

    let project_candidate = project_root.join(artifact);
    let workspace_candidate = workspace_root.join(artifact);

    let candidate = if project_candidate.exists() {
        project_candidate
    } else if workspace_candidate.exists() {
        workspace_candidate
    } else {
        return Err(ToolResult::error(format!(
            "Artifact '{}' does not exist under '{}' or workspace root '{}'",
            artifact,
            project_root.display(),
            workspace_root.display()
        )));
    };

    let canonical_workspace_root = workspace_root.canonicalize().map_err(|e| {
        ToolResult::error(format!(
            "Cannot resolve workspace root '{}': {}",
            workspace_root.display(),
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

    if !canonical_artifact.starts_with(&canonical_workspace_root) {
        return Err(ToolResult::error(format!(
            "Artifact '{}' resolves outside workspace root '{}'",
            artifact,
            workspace_root.display()
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

fn build_command(package: Option<&str>, test_name: Option<&str>) -> String {
    let mut parts = vec![
        "cargo".to_string(),
        "test".to_string(),
        "--target".to_string(),
        "wasm32-wasip1".to_string(),
    ];

    if let Some(package) = package.map(str::trim).filter(|name| !name.is_empty()) {
        parts.push("-p".to_string());
        parts.push(package.to_string());
    }

    if let Some(test_name) = test_name.map(str::trim).filter(|name| !name.is_empty()) {
        parts.push(test_name.to_string());
    }

    parts.push("--no-run".to_string());
    parts.push("--message-format=json-render-diagnostics".to_string());
    parts.push("--color".to_string());
    parts.push("never".to_string());

    parts
        .into_iter()
        .map(|part| shell_single_quote(&part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn helper_build_command() -> String {
    ["cargo", "build", "-p", "wasm_test", "--color", "never"]
        .into_iter()
        .map(shell_single_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn helper_runner_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target/debug/wasm_test")
}

fn find_package_for_project_root(project_root: &Path) -> Option<String> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(project_root)
        .no_deps()
        .exec()
        .ok()?;

    let root_manifest = project_root.join("Cargo.toml").canonicalize().ok()?;

    metadata
        .packages
        .into_iter()
        .find(|package| package.manifest_path.as_std_path() == root_manifest)
        .map(|package| package.name)
}

fn infer_package(project_root: &Path, workspace_root: &Path) -> Option<String> {
    if project_root != workspace_root {
        return find_package_for_project_root(project_root);
    }

    find_package_for_project_root(&workspace_root.join("crates/wasm_tests"))
}

fn parse_artifacts(build_output: &str) -> Vec<PathBuf> {
    let mut artifacts = Vec::new();
    for line in build_output.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if message.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }

        if message
            .pointer("/target/test")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            continue;
        }

        let Some(executable) = message
            .get("executable")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };

        if executable.ends_with(".wasm") {
            artifacts.push(PathBuf::from(executable));
        }
    }

    artifacts.sort();
    artifacts.dedup();
    artifacts
}

fn run_command(
    workspace_root: &Path,
    project_root: &Path,
    artifact: &Path,
    test_name: Option<&str>,
    args: &[String],
) -> String {
    let runner = helper_runner_path(workspace_root);
    let mut command = format!(
        "{} run-artifact {} {}",
        shell_single_quote(&runner.display().to_string()),
        shell_single_quote(&project_root.display().to_string()),
        shell_single_quote(&artifact.display().to_string())
    );

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
    let mut cmd = match firejailed_shell_command_with_extra_firejail_args(
        command,
        network_access,
        firejail_args,
    ) {
        Ok(cmd) => cmd,
        Err(err) => return ToolResult::error(err),
    };
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
        "Wasm_tests"
    }

    fn description(&self) -> &str {
        "Can safely run Rust tests that are wasm32-wasip1 compatible (no filesystem, no network). Build network permission, when needed, is asked at most once per session."
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

        let mut config_note = None;
        if !project_has_wasm_test_config(&project_root) {
            if let Err(err) = ensure_configure_permission(&project_root, ctx).await {
                return err;
            }
            let config_path = match configure_project(&project_root) {
                Ok(path) => path,
                Err(err) => return ToolResult::error(err),
            };
            config_note = Some(format!(
                "Configured wasm32-wasip1 runner at {}",
                config_path.display()
            ));
        }

        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);
        let args = input.args.unwrap_or_default();
        let test_name = input.test_name.as_deref();
        let inferred_package = infer_package(&project_root, &workspace_root);

        let explicit_artifact = match resolve_artifact_path(
            &project_root,
            &workspace_root,
            input.artifact.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => return err,
        };
        let helper_runner = helper_runner_path(&workspace_root);
        let needs_helper_build = !helper_runner.is_file();
        let needs_wasm_build = explicit_artifact.is_none();
        let needs_any_build = needs_helper_build || needs_wasm_build;

        if let Some(ref policy) = ctx.network_policy {
            if needs_any_build {
                match policy
                    .check(self.name(), BUILD_PROMPT, NetworkAccess::Full)
                    .await
                {
                    NetworkDecision::Allow(_) => {}
                    NetworkDecision::Deny(reason) => {
                        return ToolResult::error(format!("Permission denied: {}", reason));
                    }
                }
            }
        }

        let build_firejail_args =
            home_entries_and_workspace_firejail_args(&workspace_root, &[".cargo", ".rustup"]);

        let _helper_build_output = if needs_helper_build {
            let result = execute_shell_command(
                &helper_build_command(),
                &workspace_root,
                NetworkAccess::Full,
                &build_firejail_args,
                timeout_ms,
            )
            .await;

            if result.is_error {
                return ToolResult::error(format!(
                    "Failed to build wasm_test helper runner at {}:\n{}",
                    helper_runner.display(),
                    result.content
                ));
            }
            result
        } else {
            ToolResult::success(format!(
                "Skipping helper build because runner already exists: {}",
                helper_runner.display()
            ))
        };

        let build_output = if let Some(artifact) = explicit_artifact.as_ref() {
            ToolResult::success(format!(
                "Skipping wasm artifact build because explicit artifact was provided: {}",
                artifact.display()
            ))
        } else {
            let build_command = build_command(inferred_package.as_deref(), test_name);
            let result = execute_shell_command(
                &build_command,
                &project_root,
                NetworkAccess::Full,
                &build_firejail_args,
                timeout_ms,
            )
            .await;

            if result.is_error {
                return result;
            }
            result
        };

        let discovered_artifacts = match explicit_artifact.as_ref() {
            Some(path) => vec![path.clone()],
            None => parse_artifacts(&build_output.content),
        };

        if discovered_artifacts.is_empty() {
            return ToolResult::error(
                "No wasm test artifacts were discovered after building".to_string(),
            );
        }

        let run_firejail_args = read_only_workspace_firejail_args(&workspace_root, &[]);

        let selected_artifact = if let Some(artifact) = explicit_artifact {
            artifact
        } else if discovered_artifacts.len() == 1 {
            discovered_artifacts[0].clone()
        } else if let Some(test_name) = test_name.map(str::trim).filter(|name| !name.is_empty()) {
            let mut matching_artifacts = Vec::new();

            for artifact in &discovered_artifacts {
                let list_args = vec!["--list".to_string()];
                let list_command =
                    run_command(&workspace_root, &project_root, artifact, None, &list_args);
                let list_result = execute_shell_command(
                    &list_command,
                    &project_root,
                    NetworkAccess::Blocked,
                    &run_firejail_args,
                    timeout_ms,
                )
                .await;

                if list_result.is_error {
                    return list_result;
                }

                if list_result
                    .content
                    .lines()
                    .any(|line| line.trim() == format!("{test_name}: test"))
                {
                    matching_artifacts.push(artifact.clone());
                }
            }

            match matching_artifacts.len() {
                0 => {
                    return ToolResult::error(format!(
                        "Discovered {} wasm test artifacts, but none contain test '{}'. Please specify 'artifact'.\n{}",
                        discovered_artifacts.len(),
                        test_name,
                        discovered_artifacts
                            .iter()
                            .map(|artifact| artifact.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                1 => matching_artifacts.remove(0),
                _ => {
                    return ToolResult::error(format!(
                        "Test '{}' appears in multiple wasm test artifacts; please specify 'artifact'.\n{}",
                        test_name,
                        matching_artifacts
                            .iter()
                            .map(|artifact| artifact.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
            }
        } else {
            let artifacts = discovered_artifacts
                .iter()
                .map(|artifact| artifact.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return ToolResult::error(format!(
                "Discovered {} wasm test artifacts; please specify 'artifact'.\n{}",
                discovered_artifacts.len(),
                artifacts
            ));
        };

        let run_command = run_command(
            &workspace_root,
            &project_root,
            &selected_artifact,
            test_name,
            &args,
        );
        let run_output = execute_shell_command(
            &run_command,
            selected_artifact.parent().unwrap_or(project_root.as_path()),
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
        if let Some(note) = config_note.as_deref() {
            content.push_str("Configuration:\n");
            content.push_str(note);
            content.push_str("\n\n");
        }
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
            "artifact": selected_artifact,
            "project_root": project_root,
            "package": inferred_package,
            "sandbox": {
                "build_network": "enabled",
                "run_network": "blocked",
                "build_filesystem": "workspace plus cargo/rustup home entries",
                "run_filesystem": "workspace read-only"
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionPolicy;
    use parking_lot::Mutex;
    use serde_json::json;
    use std::sync::Arc;

    struct RecordingPermissionPolicy {
        decision: Mutex<PermissionDecision>,
        requests: Mutex<Vec<PermissionRequest>>,
    }

    impl RecordingPermissionPolicy {
        fn new(decision: PermissionDecision) -> Self {
            Self {
                decision: Mutex::new(decision),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().len()
        }

        fn requests(&self) -> Vec<PermissionRequest> {
            self.requests.lock().clone()
        }
    }

    #[async_trait]
    impl PermissionPolicy for RecordingPermissionPolicy {
        async fn check(&self, request: &PermissionRequest) -> PermissionDecision {
            self.requests.lock().push(request.clone());
            self.decision.lock().clone()
        }
    }

    fn test_ctx(root: &Path, policy: Arc<dyn PermissionPolicy>) -> ToolContext {
        ToolContext {
            working_dir: root.to_path_buf(),
            session_id: format!("wasm-tests-{}", uuid::Uuid::new_v4()),
            permissions: policy,
            ..ToolContext::default()
        }
    }

    fn init_project(root: &Path) {
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"wasm-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn meaning() -> u32 { 42 }\n").unwrap();
    }

    #[test]
    fn build_command_targets_inferred_package_when_provided() {
        let command = build_command(Some("wasm_tests"), Some("my_test"));
        assert!(command.contains("'-p' 'wasm_tests'"));
        assert!(command.contains("'my_test'"));
        assert!(command.contains("'--target' 'wasm32-wasip1'"));
    }

    #[test]
    fn parse_artifacts_collects_unique_wasm_test_binaries() {
        let output = r#"{"reason":"compiler-artifact","target":{"test":true},"executable":"/tmp/a.wasm"}
{"reason":"compiler-artifact","target":{"test":false},"executable":"/tmp/skip.wasm"}
{"reason":"compiler-artifact","target":{"test":true},"executable":"/tmp/a.wasm"}
{"reason":"compiler-artifact","target":{"test":true},"executable":"/tmp/b.wasm"}"#;
        let artifacts = parse_artifacts(output);
        assert_eq!(
            artifacts,
            vec![PathBuf::from("/tmp/a.wasm"), PathBuf::from("/tmp/b.wasm")]
        );
    }
    #[tokio::test]
    async fn missing_config_asks_operator_permission_before_writing() {
        let workspace = tempfile::tempdir().unwrap();
        init_project(workspace.path());
        let policy = Arc::new(RecordingPermissionPolicy::new(PermissionDecision::Deny(
            "User denied".into(),
        )));
        let ctx = test_ctx(workspace.path(), policy.clone());
        let tool = WasmTestsTool;

        let result = tool
            .execute(json!({ "artifact": "missing.wasm" }), &ctx)
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, "Permission denied: User denied");
        assert!(result.metadata.is_none());
        assert_eq!(policy.request_count(), 1);

        let requests = policy.requests();
        assert_eq!(requests[0].tool_name, "Wasm_tests");
        assert_eq!(requests[0].permission_level, PermissionLevel::Write);
        assert!(requests[0]
            .description
            .contains("Configure Wasm_tests runner"));
        assert!(!wasm_test_config_path(workspace.path()).exists());
    }

    #[tokio::test]
    async fn allowed_config_write_updates_workspace_before_continuing() {
        let workspace = tempfile::tempdir().unwrap();
        init_project(workspace.path());
        let policy = Arc::new(RecordingPermissionPolicy::new(PermissionDecision::Allow));
        let ctx = test_ctx(workspace.path(), policy.clone());
        let tool = WasmTestsTool;

        let result = tool
            .execute(json!({ "artifact": "missing.wasm" }), &ctx)
            .await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("Artifact 'missing.wasm' does not exist"));
        assert_eq!(policy.request_count(), 1);

        let config_path = wasm_test_config_path(workspace.path());
        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("wasm32-wasip1"));
        assert!(config.contains("runner = \"wasm_test\""));
    }
}
