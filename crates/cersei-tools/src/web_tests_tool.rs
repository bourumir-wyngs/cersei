//! web_tests tool: run constrained web test and validation commands.

use super::*;
use crate::network_policy::{
    firejailed_shell_command_with_extra_firejail_args, NetworkAccess, NetworkDecision,
};
use crate::shell_sandbox::{
    home_entries_and_workspace_firejail_args, resolve_directory_in_workspace,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::process::Stdio;

const WEB_TESTS_REGEX_1: &str = r"^(?:npm|pnpm|yarn|bun) (?:run )?(?:test|test:unit|test:ui|test:web|test:frontend|lint|lint:js|lint:ts|lint:tsx|typecheck|check)$";
const WEB_TESTS_REGEX_2: &str = r"^(?:(?:npx (?:\-\-yes )?)|pnpm exec|yarn|bunx) (?:vitest(?: run| watch)?|jest|playwright test|cypress run|eslint|tsc|vite(?: build|preview)?|svelte-check|prettier|stylelint)(?: [A-Za-z0-9_./:@\-=]+)*$";

static WEB_TESTS_REGEXES: Lazy<[Regex; 2]> = Lazy::new(|| {
    [
        Regex::new(WEB_TESTS_REGEX_1).expect("valid web_tests regex 1"),
        Regex::new(WEB_TESTS_REGEX_2).expect("valid web_tests regex 2"),
    ]
});

pub struct WebTestsTool;

fn command_matches_supported_regexes(command: &str) -> bool {
    WEB_TESTS_REGEXES
        .iter()
        .any(|regex| regex.is_match(command))
        || command
            .strip_prefix("npx --yes ")
            .map(|rest| format!("npx --yes  {rest}"))
            .is_some_and(|normalized| {
                WEB_TESTS_REGEXES
                    .iter()
                    .any(|regex| regex.is_match(&normalized))
            })
}

pub(crate) fn is_supported_web_test_command(command: &str) -> bool {
    let command = command.trim();
    !command.is_empty() && command_matches_supported_regexes(command)
}

fn invalid_web_tests_command_message() -> String {
    format!(
        "command must be provided and matched regular expressions \n- regex: {WEB_TESTS_REGEX_1}\n- regex: {WEB_TESTS_REGEX_2}"
    )
}

pub(crate) fn redirect_to_web_tests_error(tool_name: &str, command: &str) -> Option<String> {
    is_supported_web_test_command(command).then_some(format!(
        "Action denied, do not use {} for '{}', use web_tests. If does not do what you want or is buggy, report to the user.",
        tool_name,
        command.trim()
    ))
}

fn validate_command(command: Option<&str>) -> std::result::Result<String, ToolResult> {
    let command = command.map(str::trim).filter(|command| !command.is_empty());
    match command {
        Some(command) if is_supported_web_test_command(command) => Ok(command.to_string()),
        _ => Err(ToolResult::error(invalid_web_tests_command_message())),
    }
}

#[async_trait]
impl Tool for WebTestsTool {
    fn name(&self) -> &str {
        "Web_tests"
    }

    fn description(&self) -> &str {
        "Run constrained web test, lint, and typecheck commands. Network is always disabled. The working directory stays inside the workspace and the sandbox exposes the workspace plus local Node environment paths."
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
                "command": {
                    "type": "string",
                    "description": "Required command matching the fixed web test regex set, e.g. \"npm run test:web\" or \"npx --yes eslint src/app.tsx\"."
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional subdirectory (relative to the working root) in which to run the command. Must not escape the root directory."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (max 600000)"
                }
            }
        })
    }

    fn preflight(&self, input: &Value, _ctx: &ToolContext) -> Option<ToolResult> {
        validate_command(input.get("command").and_then(Value::as_str)).err()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            command: Option<String>,
            workdir: Option<String>,
            timeout: Option<u64>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let command = match validate_command(input.command.as_deref()) {
            Ok(command) => command,
            Err(err) => return err,
        };

        let shell_state = session_shell_state(&ctx.session_id);
        let base_cwd = {
            let state = shell_state.lock();
            state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone())
        };

        let (cwd, workspace_root) = match resolve_directory_in_workspace(
            &base_cwd,
            input
                .workdir
                .as_deref()
                .map(str::trim)
                .filter(|dir| !dir.is_empty()),
            &ctx.working_dir,
            "web_tests",
        ) {
            Ok(paths) => paths,
            Err(err) => return ToolResult::error(err),
        };

        if let Some(ref policy) = ctx.network_policy {
            match policy
                .check(self.name(), &command, NetworkAccess::Blocked)
                .await
            {
                NetworkDecision::Allow(_) => {}
                NetworkDecision::Deny(reason) => {
                    return ToolResult::error(format!("Permission denied: {}", reason));
                }
            }
        }

        let timeout_ms = input.timeout.unwrap_or(120_000).min(600_000);
        let firejail_args =
            home_entries_and_workspace_firejail_args(&workspace_root, &[".npm", ".npmrc", ".nvm"]);
        let mut cmd = match firejailed_shell_command_with_extra_firejail_args(
            &command,
            NetworkAccess::Blocked,
            &firejail_args,
        ) {
            Ok(cmd) => cmd,
            Err(err) => return ToolResult::error(err),
        };
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
                        ToolResult::success("(web_tests completed with no output)")
                    } else {
                        ToolResult::success(content)
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    ToolResult::error(format!("Exit code {}\n{}", code, content))
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Failed to execute web_tests: {}", e)),
            Err(_) => ToolResult::error(format!("web_tests timed out after {}ms", timeout_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::Mutex;

    static PATH_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    struct PathGuard(Option<std::ffi::OsString>);

    impl PathGuard {
        fn prepend(dir: &std::path::Path) -> Self {
            let previous = std::env::var_os("PATH");
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(std::env::split_paths(
                previous.as_deref().unwrap_or_default(),
            ));
            let joined = std::env::join_paths(paths).expect("join PATH");
            std::env::set_var("PATH", &joined);
            Self(previous)
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    fn test_ctx(session_id: String, working_dir: std::path::PathBuf) -> ToolContext {
        ToolContext {
            session_id,
            working_dir,
            permissions: Arc::new(permissions::AllowAll),
            ..ToolContext::default()
        }
    }

    #[test]
    fn validates_supported_commands() {
        assert!(is_supported_web_test_command("npm run test:web"));
        assert!(is_supported_web_test_command(
            "npx --yes eslint src/app.tsx"
        ));
        assert!(!is_supported_web_test_command("npm install"));
        assert!(!is_supported_web_test_command("npx cowsay hi"));
    }

    #[test]
    fn preflight_rejects_missing_or_invalid_command() {
        let tool = WebTestsTool;

        let missing = tool
            .preflight(&serde_json::json!({}), &ToolContext::default())
            .expect("missing command should be rejected");
        assert!(missing.is_error);
        assert!(missing
            .content
            .contains("command must be provided and matched regular expressions"));
        assert!(missing.content.contains(WEB_TESTS_REGEX_1));
        assert!(missing.content.contains(WEB_TESTS_REGEX_2));

        let invalid = tool
            .preflight(
                &serde_json::json!({ "command": "npm install" }),
                &ToolContext::default(),
            )
            .expect("invalid command should be rejected");
        assert!(invalid.is_error);
        assert!(invalid.content.contains(WEB_TESTS_REGEX_1));
        assert!(invalid.content.contains(WEB_TESTS_REGEX_2));
    }

    #[test]
    fn web_tests_tool_uses_testing_category() {
        let tool = WebTestsTool;
        assert_eq!(tool.category(), ToolCategory::Testing);
    }

    #[tokio::test]
    async fn execute_runs_valid_command_in_workspace() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("package.json"), "{}").expect("package.json");

        let ctx = test_ctx(
            format!("web-tests-{}", uuid::Uuid::new_v4()),
            workspace.path().to_path_buf(),
        );
        let tool = WebTestsTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "command": "npm run test:web",
                    "timeout": 1000
                }),
                &ctx,
            )
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("Exit code") || result.content.contains("Failed to execute")
        );
    }

    #[tokio::test]
    async fn execute_uses_workdir_for_web_tests_command() {
        let _lock = PATH_LOCK.lock().expect("PATH lock");
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_bin = workspace.path().join(".fake-bin");
        fs::create_dir_all(&fake_bin).expect("fake bin");

        let npm_path = fake_bin.join("npm");
        fs::write(
            &npm_path,
            "#!/bin/sh\nprintf 'cwd=%s\\n' \"$PWD\"\nprintf 'args=%s\\n' \"$*\"\n",
        )
        .expect("script");
        let mut perms = fs::metadata(&npm_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&npm_path, perms).expect("chmod");

        let subdir = workspace.path().join("frontend");
        fs::create_dir_all(&subdir).expect("subdir");

        let _path_guard = PathGuard::prepend(&fake_bin);
        let ctx = test_ctx(
            format!("web-tests-workdir-{}", uuid::Uuid::new_v4()),
            workspace.path().to_path_buf(),
        );
        let tool = WebTestsTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "command": "npm run test:web",
                    "workdir": "frontend"
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result
            .content
            .contains(&format!("cwd={}", subdir.display())));
        assert!(result.content.contains("args=run test:web"));
    }

    #[tokio::test]
    async fn execute_rejects_removed_directory_parameter() {
        let workspace = tempfile::tempdir().expect("workspace");
        let ctx = test_ctx(
            format!("web-tests-removed-directory-{}", uuid::Uuid::new_v4()),
            workspace.path().to_path_buf(),
        );
        let tool = WebTestsTool;

        let result = tool
            .execute(
                serde_json::json!({
                    "command": "npm run test:web",
                    "directory": "frontend"
                }),
                &ctx,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("unknown field `directory`"));
    }
}
