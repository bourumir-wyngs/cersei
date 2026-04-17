//! Pytest tool: run pytest from a workspace-local virtual environment.

use super::*;
use crate::network_policy::{
    firejailed_shell_command_with_extra_firejail_args, NetworkAccess, NetworkDecision,
};
use crate::shell_sandbox::{read_only_workspace_firejail_args, resolve_directory_in_workspace};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;

const PYTEST_CACHE_DIR: &str = ".pytest_cache";
const PYTHON_PYCACHE_PREFIX_REL: &str = ".pytest_cache/pycache";

pub struct PytestTool;

fn shell_single_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

fn expected_pytest_binary_path(venv_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        venv_dir.join("Scripts").join("pytest.exe")
    } else {
        venv_dir.join("bin").join("pytest")
    }
}

fn workspace_venv_dir(cwd: &Path, workspace_root: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        if !dir.starts_with(workspace_root) {
            continue;
        }

        let candidate = dir.join(".venv");
        if candidate.is_dir() {
            return Some(candidate);
        }

        if dir == workspace_root {
            break;
        }
    }

    None
}

fn virtual_environment_error(expected_binary: &Path) -> ToolResult {
    ToolResult::error(format!(
        "virtual enviroment not configured: expected pytest at '{}'",
        expected_binary.display()
    ))
}

fn resolve_pytest_binary(
    cwd: &Path,
    workspace_root: &Path,
) -> std::result::Result<PathBuf, ToolResult> {
    let venv_dir =
        workspace_venv_dir(cwd, workspace_root).unwrap_or_else(|| workspace_root.join(".venv"));
    let expected_binary = expected_pytest_binary_path(&venv_dir);

    if !expected_binary.is_file() {
        return Err(virtual_environment_error(&expected_binary));
    }

    let canonical_binary = expected_binary
        .canonicalize()
        .map_err(|_| virtual_environment_error(&expected_binary))?;
    if !canonical_binary.starts_with(workspace_root) {
        return Err(virtual_environment_error(&expected_binary));
    }

    Ok(canonical_binary)
}

#[async_trait]
impl Tool for PytestTool {
    fn name(&self) -> &str {
        "Pytest"
    }

    fn description(&self) -> &str {
        "Run pytest from a workspace-local .venv. Network is always disabled. The workspace is read-only except for pytest/python cache writes under .pytest_cache."
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
                    "description": "Optional arguments to pass to pytest, e.g. \"-q\", \"tests/unit -k parser\". Omit to run plain pytest."
                },
                "directory": {
                    "type": "string",
                    "description": "Optional subdirectory (relative to the working root) in which to run pytest. Must not escape the root directory."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (max 600000)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            args: Option<String>,
            directory: Option<String>,
            timeout: Option<u64>,
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

        let (cwd, workspace_root) = match resolve_directory_in_workspace(
            &base_cwd,
            input.directory.as_deref(),
            &ctx.working_dir,
            "pytest",
        ) {
            Ok(paths) => paths,
            Err(err) => return ToolResult::error(err),
        };

        let pytest_binary = match resolve_pytest_binary(&cwd, &workspace_root) {
            Ok(path) => path,
            Err(err) => return err,
        };

        let pycache_prefix = workspace_root.join(PYTHON_PYCACHE_PREFIX_REL);
        if let Err(err) = std::fs::create_dir_all(&pycache_prefix) {
            return ToolResult::error(format!(
                "Failed to prepare pytest cache directory '{}': {}",
                pycache_prefix.display(),
                err
            ));
        }

        if let Some(ref policy) = ctx.network_policy {
            match policy
                .check(
                    self.name(),
                    &format!("pytest {}", input.args.clone().unwrap_or_default()),
                    NetworkAccess::Blocked,
                )
                .await
            {
                NetworkDecision::Allow(_) => {}
                NetworkDecision::Deny(reason) => {
                    return ToolResult::error(format!("Permission denied: {}", reason));
                }
            }
        }

        let args = input.args.unwrap_or_default();
        let quoted_binary = shell_single_quote(&pytest_binary.display().to_string());
        let command = if args.trim().is_empty() {
            quoted_binary
        } else {
            format!("{quoted_binary} {args}")
        };
        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);
        let firejail_args = read_only_workspace_firejail_args(&workspace_root, &[PYTEST_CACHE_DIR]);
        let mut cmd = firejailed_shell_command_with_extra_firejail_args(
            &command,
            NetworkAccess::Blocked,
            &firejail_args,
        );
        cmd.current_dir(&cwd)
            .env("PYTHONPYCACHEPREFIX", &pycache_prefix)
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
                        ToolResult::success("(pytest completed with no output)")
                    } else {
                        ToolResult::success(content)
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    ToolResult::error(format!("Exit code {}\n{}", code, content))
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Failed to execute pytest: {}", e)),
            Err(_) => ToolResult::error(format!("pytest timed out after {}ms", timeout_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    fn test_ctx(session_id: String, working_dir: PathBuf) -> ToolContext {
        ToolContext {
            session_id,
            working_dir,
            permissions: Arc::new(permissions::AllowAll),
            ..ToolContext::default()
        }
    }

    #[test]
    fn reports_missing_virtual_environment_with_expected_path() {
        let workspace = tempfile::tempdir().expect("workspace");
        let expected = workspace.path().join(".venv").join("bin").join("pytest");

        let err = resolve_pytest_binary(workspace.path(), workspace.path()).unwrap_err();
        assert_eq!(
            err.content,
            format!(
                "virtual enviroment not configured: expected pytest at '{}'",
                expected.display()
            )
        );
    }

    #[test]
    fn reports_missing_pytest_binary_inside_detected_venv() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(workspace.path().join(".venv").join("bin")).expect("venv");

        let expected = workspace.path().join(".venv").join("bin").join("pytest");
        let err = resolve_pytest_binary(workspace.path(), workspace.path()).unwrap_err();
        assert_eq!(
            err.content,
            format!(
                "virtual enviroment not configured: expected pytest at '{}'",
                expected.display()
            )
        );
    }

    #[tokio::test]
    async fn execute_runs_workspace_venv_pytest_with_cache_redirected() {
        let workspace = tempfile::tempdir().expect("workspace");
        let bin_dir = workspace.path().join(".venv").join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");

        let pytest_path = bin_dir.join("pytest");
        fs::write(
            &pytest_path,
            "#!/bin/sh\nprintf 'cwd=%s\\n' \"$PWD\"\nprintf 'pycache=%s\\n' \"$PYTHONPYCACHEPREFIX\"\nprintf 'args=%s\\n' \"$*\"\n",
        )
        .expect("script");
        let mut perms = fs::metadata(&pytest_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&pytest_path, perms).expect("chmod");

        let subdir = workspace.path().join("tests");
        fs::create_dir_all(&subdir).expect("subdir");

        let ctx = test_ctx(
            format!("pytest-test-{}", uuid::Uuid::new_v4()),
            workspace.path().to_path_buf(),
        );
        let tool = PytestTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "directory": "tests",
                    "args": "-q tests/unit"
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result
            .content
            .contains(&format!("cwd={}", subdir.display())));
        assert!(result.content.contains(&format!(
            "pycache={}",
            workspace.path().join(PYTHON_PYCACHE_PREFIX_REL).display()
        )));
        assert!(result.content.contains("args=-q tests/unit"));
        assert!(workspace.path().join(PYTHON_PYCACHE_PREFIX_REL).is_dir());
    }
}
