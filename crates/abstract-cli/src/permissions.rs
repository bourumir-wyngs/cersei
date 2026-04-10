//!
//! Session decisions are cached in memory. Persisted decisions live in
//! `~/.abstract/permissions_<project>.yaml` as regex-based rules that are
//! reloaded on every permission check.

use crate::config;
use crate::theme::Theme;
use cersei_tools::network_policy::NetworkAccess;
use cersei_tools::permissions::{PermissionDecision, PermissionPolicy, PermissionRequest};
use cersei_tools::PermissionLevel;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetAttribute, SetForegroundColor};
use parking_lot::Mutex;
use regex::Regex;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_REVIEW_PREVIEW_LINES: usize = 5;
const MAX_REVIEW_PREVIEW_CHARS: usize = 512;

/// Interactive permission policy for the CLI.
/// Prompts user for Write/Execute/Dangerous tools, auto-allows ReadOnly/None
/// unless an explicit persisted rule says otherwise.
pub struct CliPermissionPolicy {
    /// Commands allowed for the entire session.
    session_allowed: Mutex<HashSet<String>>,
    /// Commands denied for the entire session.
    session_denied: Mutex<HashSet<String>>,
    theme: Theme,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedPermissionRule {
    pub(crate) regex: String,
    #[serde(default)]
    pub(crate) network: bool,
    #[serde(default)]
    pub(crate) allow: bool,
    #[serde(default)]
    pub(crate) allow_read: Vec<String>,
}

type PersistedPermissions = Vec<PersistedPermissionRule>;

pub(crate) fn command_line_from_request(request: &PermissionRequest) -> String {
    command_line_from_tool_input(&request.tool_name, &request.tool_input)
}

pub(crate) fn command_line_from_tool_input(
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> String {
    let direct_command = || {
        tool_input
            .get("command")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };

    match tool_name {
        "Bash" | "bash" | "Process" | "PowerShell" => direct_command(),
        "Cargo" => tool_input
            .get("args")
            .and_then(|value| value.as_str())
            .map(|args| format!("cargo {args}")),
        "Npm" => tool_input
            .get("args")
            .and_then(|value| value.as_str())
            .map(|args| format!("npm {args}")),
        "Npx" => tool_input
            .get("args")
            .and_then(|value| value.as_str())
            .map(|args| format!("npx --yes {args}")),
        _ => direct_command(),
    }
    .or_else(|| compact_tool_input(tool_input).map(|input| format!("{tool_name} {input}")))
    .unwrap_or_else(|| tool_name.to_string())
}

pub(crate) fn match_persisted_rule_for_request(
    command_line: &str,
    uses_network: bool,
) -> Option<PersistedPermissionRule> {
    let persisted = load_persisted_file();
    match_persisted_rule_for_request_in(&persisted, command_line, uses_network).cloned()
}

pub(crate) fn match_persisted_rule_for_request_in<'a>(
    persisted: &'a [PersistedPermissionRule],
    command_line: &str,
    uses_network: bool,
) -> Option<&'a PersistedPermissionRule> {
    persisted.iter().find(|rule| {
        if !uses_network && rule.network {
            return false;
        }
        Regex::new(&rule.regex)
            .ok()
            .is_some_and(|regex| regex.is_match(command_line))
    })
}

fn request_uses_network(request: &PermissionRequest) -> bool {
    match request.tool_name.as_str() {
        "Cargo" | "Npm" | "Npx" | "Bash" | "bash" | "Process" | "PowerShell" => {
            let network = request
                .tool_input
                .get("network")
                .and_then(|value| value.as_str());
            NetworkAccess::from_input(network) != NetworkAccess::Blocked
        }
        _ => false,
    }
}

pub(crate) fn register_command_line(command_line: &str) {
    let path = config::permissions_path();
    let mut persisted = load_persisted_file_from(&path);
    let regex = exact_command_line_regex(command_line);

    if persisted.iter().any(|rule| rule.regex == regex) {
        return;
    }

    persisted.push(PersistedPermissionRule {
        regex,
        network: false,
        allow: false,
        allow_read: Vec::new(),
    });
    save_persisted_file_to(&path, &persisted);
}

impl CliPermissionPolicy {
    pub fn new(theme: &Theme) -> Self {
        Self {
            session_allowed: Mutex::new(HashSet::new()),
            session_denied: Mutex::new(HashSet::new()),
            theme: theme.clone(),
        }
    }
}

#[async_trait::async_trait]
impl PermissionPolicy for CliPermissionPolicy {
    async fn check(&self, request: &PermissionRequest) -> PermissionDecision {
        if request.permission_level == PermissionLevel::Forbidden {
            return PermissionDecision::Deny("Operation is forbidden".into());
        }

        let command_line = command_line_from_request(request);
        let persisted = load_persisted_file();
        let request_uses_network = request_uses_network(request);

        if let Some(rule) =
            match_persisted_rule_for_request_in(&persisted, &command_line, request_uses_network)
        {
            return if rule.allow {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny("User denied (registered rule)".into())
            };
        }

        match request.permission_level {
            PermissionLevel::None => {
                return PermissionDecision::Allow;
            }
            PermissionLevel::ReadOnly => {
                if read_only_targets_allowed(request, &persisted).unwrap_or(true) {
                    return PermissionDecision::Allow;
                }
            }
            _ => {}
        }

        if self.session_denied.lock().contains(&command_line) {
            return PermissionDecision::Deny("User denied (session)".into());
        }
        if self.session_allowed.lock().contains(&command_line) {
            return PermissionDecision::Allow;
        }

        let level_str = format!("{:?}", request.permission_level);
        let preview = permission_preview(request, &command_line);

        loop {
            let _ = execute!(
                io::stderr(),
                Print("\n"),
                SetForegroundColor(self.theme.permission_accent),
                SetAttribute(crossterm::style::Attribute::Bold),
                Print(format!("  Permission required: {}", request.tool_name)),
                ResetColor,
                SetAttribute(crossterm::style::Attribute::Reset),
                Print("\n"),
                SetForegroundColor(self.theme.dim),
                Print(format!("  {}", request.description)),
                ResetColor,
                Print("\n"),
            );
            if let Some(preview) = &preview {
                let _ = execute!(
                    io::stderr(),
                    SetForegroundColor(self.theme.review_text),
                    Print(indent_review_text(preview)),
                    ResetColor,
                    Print("\n"),
                );
            }
            let _ = execute!(
                io::stderr(),
                SetForegroundColor(self.theme.dim),
                Print(format!("  Risk: {level_str}")),
                ResetColor,
                Print("\n"),
                SetForegroundColor(self.theme.permission_accent),
                Print("  [Y]es  [N]o  n[E]ver  Deny e[X]plaining  [S]ession  [R]egister "),
                ResetColor,
            );
            let _ = io::stderr().flush();

            match read_permission_char() {
                'y' | 'Y' | '\n' => return PermissionDecision::AllowOnce,
                's' | 'S' => {
                    self.session_allowed.lock().insert(command_line.clone());
                    return PermissionDecision::AllowForSession;
                }
                'e' | 'E' => {
                    self.session_denied.lock().insert(command_line.clone());
                    return PermissionDecision::Deny("User denied (session)".into());
                }
                'r' | 'R' => {
                    register_command_line(&command_line);
                    continue;
                }
                'x' | 'X' => {
                    let _ = execute!(
                        io::stderr(),
                        SetForegroundColor(self.theme.permission_accent),
                        Print("\n  Why denied? "),
                        ResetColor,
                    );
                    let _ = io::stderr().flush();
                    let mut explanation = String::new();
                    let _ = io::stdin().read_line(&mut explanation);
                    return PermissionDecision::Deny(format!(
                        "User denied: {}",
                        explanation.trim()
                    ));
                }
                _ => return PermissionDecision::Deny("User denied".into()),
            }
        }
    }
}

fn read_only_targets_allowed(
    request: &PermissionRequest,
    persisted: &[PersistedPermissionRule],
) -> Option<bool> {
    if request.permission_level != PermissionLevel::ReadOnly {
        return None;
    }

    let targets = extract_read_targets(request)?;
    if targets.is_empty() {
        return Some(true);
    }

    let workspace_root = normalize_path(&request.working_dir, &request.working_dir);
    let allow_read_roots: Vec<PathBuf> = persisted
        .iter()
        .flat_map(|rule| rule.allow_read.iter())
        .map(|path| normalize_path(Path::new(path), &request.working_dir))
        .collect();

    Some(targets.iter().all(|target| {
        path_within(target, &workspace_root)
            || allow_read_roots
                .iter()
                .any(|allowed_root| path_within(target, allowed_root))
    }))
}

fn extract_read_targets(request: &PermissionRequest) -> Option<Vec<PathBuf>> {
    let tool_input = &request.tool_input;
    let path_field = |name: &str| tool_input.get(name).and_then(|value| value.as_str());

    match request.tool_name.as_str() {
        "Read" => Some(vec![resolve_request_path(
            path_field("file_path")?,
            &request.working_dir,
        )]),
        "Glob" => Some(vec![resolve_request_path(
            path_field("path").unwrap_or("."),
            &request.working_dir,
        )]),
        "Grep" => Some(vec![resolve_request_path(
            path_field("path").unwrap_or("."),
            &request.working_dir,
        )]),
        "ListDirectory" => Some(vec![resolve_request_path(
            path_field("path").unwrap_or("."),
            &request.working_dir,
        )]),
        "NotebookEdit" => Some(vec![resolve_request_path(
            path_field("file_path")?,
            &request.working_dir,
        )]),
        "Write" => Some(vec![resolve_request_path(
            path_field("file_path")?,
            &request.working_dir,
        )]),
        "FileHistory" => path_field("file_path")
            .map(|path| vec![resolve_request_path(path, &request.working_dir)]),
        _ => None,
    }
}

fn resolve_request_path(path: &str, working_dir: &Path) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        normalize_path(candidate, working_dir)
    } else {
        normalize_path(&working_dir.join(candidate), working_dir)
    }
}

fn normalize_path(path: &Path, base: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        canonical
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn path_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn permission_preview(request: &PermissionRequest, command_line: &str) -> Option<String> {
    let direct_command = matches!(
        request.tool_name.as_str(),
        "Bash" | "bash" | "Process" | "PowerShell" | "Cargo" | "Npm" | "Npx"
    );

    if direct_command {
        return Some(truncate_review_text(command_line));
    }

    compact_tool_input(&request.tool_input).map(|input| truncate_review_text(&input))
}

fn truncate_review_text(s: &str) -> String {
    let original_line_count = s.lines().count();
    let mut lines: Vec<&str> = s.lines().take(MAX_REVIEW_PREVIEW_LINES + 1).collect();
    let truncated_by_lines = if original_line_count > MAX_REVIEW_PREVIEW_LINES {
        lines.truncate(MAX_REVIEW_PREVIEW_LINES);
        true
    } else {
        false
    };

    let mut out = lines.join("\n");
    let mut truncated = truncated_by_lines;
    if out.chars().count() > MAX_REVIEW_PREVIEW_CHARS {
        out = out.chars().take(MAX_REVIEW_PREVIEW_CHARS).collect();
        truncated = true;
    }
    if truncated {
        out.push_str("\n…");
    }
    out
}

fn indent_review_text(s: &str) -> String {
    s.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_tool_input(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        _ => serde_json::to_string_pretty(value).ok(),
    }
}

fn exact_command_line_regex(command_line: &str) -> String {
    format!("^{}$", regex::escape(command_line))
}

fn load_persisted_file() -> PersistedPermissions {
    load_persisted_file_from(&config::permissions_path())
}

fn load_persisted_file_from(path: &Path) -> PersistedPermissions {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_saphyr::from_str::<PersistedPermissions>(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_persisted_file_to(path: &Path, persisted: &PersistedPermissions) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(yaml) = serde_saphyr::to_string(persisted) {
        let _ = std::fs::write(path, yaml);
    }
}

fn read_permission_char() -> char {
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_ok() {
        line.chars().next().unwrap_or('\n')
    } else {
        '\n'
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_permissions_path() -> PathBuf {
        tempfile::NamedTempFile::new()
            .unwrap()
            .into_temp_path()
            .to_path_buf()
    }

    fn make_request(tool_name: &str, input: serde_json::Value) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool_name.into(),
            tool_input: input,
            permission_level: PermissionLevel::ReadOnly,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn direct_command_lines_are_extracted() {
        assert_eq!(
            command_line_from_request(&make_request("Bash", json!({ "command": "echo hi" }))),
            "echo hi"
        );
        assert_eq!(
            command_line_from_request(&make_request("Process", json!({ "command": "sleep 1" }))),
            "sleep 1"
        );
        assert_eq!(
            command_line_from_request(&make_request("Cargo", json!({ "args": "test" }))),
            "cargo test"
        );
        assert_eq!(
            command_line_from_request(&make_request("Npm", json!({ "args": "run build" }))),
            "npm run build"
        );
        assert_eq!(
            command_line_from_request(&make_request("Npx", json!({ "args": "jest --runInBand" }))),
            "npx --yes jest --runInBand"
        );
    }

    #[test]
    fn unhandled_tools_fall_back_to_json_preview() {
        let request = make_request(
            "CustomTool",
            json!({ "file": "src/main.rs", "mode": "safe" }),
        );
        let line = command_line_from_request(&request);
        assert!(line.starts_with("CustomTool "));
        assert!(line.contains("src/main.rs"));
    }

    #[test]
    fn register_command_line_adds_exact_denial_once() {
        let initial = vec![PersistedPermissionRule {
            regex: "^cargo test$".into(),
            network: false,
            allow: false,
            allow_read: Vec::new(),
        }];
        let path = temp_permissions_path();
        save_persisted_file_to(&path, &initial);

        // Simulate register logic against a temp file.
        let mut persisted = load_persisted_file_from(&path);
        let regex = exact_command_line_regex("cargo build");
        if !persisted.iter().any(|rule| rule.regex == regex) {
            persisted.push(PersistedPermissionRule {
                regex,
                network: false,
                allow: false,
                allow_read: Vec::new(),
            });
            save_persisted_file_to(&path, &persisted);
        }

        let loaded = load_persisted_file_from(&path);
        let cargo_build_rules = loaded
            .iter()
            .filter(|rule| rule.regex == "^cargo build$")
            .count();
        assert_eq!(cargo_build_rules, 1);
    }

    #[test]
    fn yaml_rules_are_respected_with_network_flag() {
        let rules = vec![
            PersistedPermissionRule {
                regex: "^cargo build$".into(),
                network: true,
                allow: true,
                allow_read: Vec::new(),
            },
            PersistedPermissionRule {
                regex: "^cargo build$".into(),
                network: false,
                allow: false,
                allow_read: Vec::new(),
            },
        ];
        let path = temp_permissions_path();
        save_persisted_file_to(&path, &rules);

        let matched = load_persisted_file_from(&path)
            .into_iter()
            .find_map(|rule| {
                if rule.network {
                    return None;
                }
                let regex = Regex::new(&rule.regex).ok()?;
                regex.is_match("cargo build").then_some(rule.allow)
            });

        assert_eq!(matched, Some(false));
    }

    #[test]
    fn networked_execute_rules_accept_network_entries() {
        let rules = vec![PersistedPermissionRule {
            regex: r"^cargo (build|check|run|test|bench|rustc|doc|rustdoc)( .*)?$".into(),
            network: true,
            allow: true,
            allow_read: Vec::new(),
        }];
        let request = PermissionRequest {
            tool_name: "Cargo".into(),
            tool_input: json!({ "args": "test", "network": "full" }),
            permission_level: PermissionLevel::Execute,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: PathBuf::from("/tmp"),
        };

        let command_line = command_line_from_request(&request);
        let matched = match_persisted_rule_for_request_in(
            &rules,
            &command_line,
            request_uses_network(&request),
        );

        assert_eq!(matched.map(|rule| rule.allow), Some(true));
    }

    #[test]
    fn networked_execute_rules_can_run_without_network() {
        let rules = vec![PersistedPermissionRule {
            regex: r"^cargo (build|check|run|test|bench|rustc|doc|rustdoc)( .*)?$".into(),
            network: false,
            allow: true,
            allow_read: Vec::new(),
        }];
        let request = PermissionRequest {
            tool_name: "Cargo".into(),
            tool_input: json!({ "args": "test --workspace" }),
            permission_level: PermissionLevel::Execute,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: PathBuf::from("/tmp"),
        };

        let command_line = command_line_from_request(&request);
        let matched = match_persisted_rule_for_request_in(
            &rules,
            &command_line,
            request_uses_network(&request),
        );

        assert_eq!(
            matched.map(|rule| (rule.network, rule.allow)),
            Some((false, true))
        );
    }

    #[test]
    fn networked_execute_rules_respect_file_order() {
        let rules = vec![
            PersistedPermissionRule {
                regex: r"^cargo (build|check|run|test|bench|rustc|doc|rustdoc)( .*)?$".into(),
                network: false,
                allow: false,
                allow_read: Vec::new(),
            },
            PersistedPermissionRule {
                regex: r"^cargo (build|check|run|test|bench|rustc|doc|rustdoc)( .*)?$".into(),
                network: true,
                allow: true,
                allow_read: Vec::new(),
            },
        ];
        let request = PermissionRequest {
            tool_name: "Cargo".into(),
            tool_input: json!({ "args": "test", "network": "full" }),
            permission_level: PermissionLevel::Execute,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: PathBuf::from("/tmp"),
        };

        let command_line = command_line_from_request(&request);
        let matched = match_persisted_rule_for_request_in(
            &rules,
            &command_line,
            request_uses_network(&request),
        );

        assert_eq!(
            matched.map(|rule| (rule.network, rule.allow)),
            Some((false, false))
        );
    }

    #[test]
    fn non_networked_execute_rules_ignore_networked_entries() {
        let rules = vec![PersistedPermissionRule {
            regex: r"^cargo (build|check|run|test|bench|rustc|doc|rustdoc)( .*)?$".into(),
            network: true,
            allow: true,
            allow_read: Vec::new(),
        }];
        let request = PermissionRequest {
            tool_name: "Cargo".into(),
            tool_input: json!({ "args": "test --workspace", "network": "none" }),
            permission_level: PermissionLevel::Execute,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: PathBuf::from("/tmp"),
        };

        let command_line = command_line_from_request(&request);
        let matched = match_persisted_rule_for_request_in(
            &rules,
            &command_line,
            request_uses_network(&request),
        );

        assert!(matched.is_none());
    }

    #[test]
    fn read_only_targets_within_workspace_are_allowed() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap();

        let request = PermissionRequest {
            tool_name: "Read".into(),
            tool_input: json!({ "file_path": file.display().to_string() }),
            permission_level: PermissionLevel::ReadOnly,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: workspace.path().to_path_buf(),
        };

        assert_eq!(read_only_targets_allowed(&request, &[]), Some(true));
    }

    #[test]
    fn read_only_targets_outside_workspace_need_allow_read() {
        let workspace = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let file = external.path().join("lib.rs");
        std::fs::write(&file, "pub fn demo() {}").unwrap();

        let request = PermissionRequest {
            tool_name: "Read".into(),
            tool_input: json!({ "file_path": file.display().to_string() }),
            permission_level: PermissionLevel::ReadOnly,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: workspace.path().to_path_buf(),
        };

        assert_eq!(read_only_targets_allowed(&request, &[]), Some(false));

        let rules = vec![PersistedPermissionRule {
            regex: "^$".into(),
            network: false,
            allow: false,
            allow_read: vec![external.path().display().to_string()],
        }];

        assert_eq!(read_only_targets_allowed(&request, &rules), Some(true));
    }

    #[test]
    fn exact_regex_is_escaped_and_anchored() {
        assert_eq!(
            exact_command_line_regex("cargo test -- --exact foo::bar"),
            r"^cargo test \-\- \-\-exact foo::bar$"
        );
    }
}
