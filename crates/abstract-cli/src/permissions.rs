//! Interactive permission UI for the CLI.
//!
//! Session decisions are cached in memory. Persisted decisions live in
//! `~/.abstract/permissions_<project>.yaml` as regex-based rules that are
//! reloaded on every permission check.

use crate::config;
use crate::theme::Theme;
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
    regex: String,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    allow: bool,
    #[serde(default)]
    allow_read: Vec<String>,
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

pub(crate) fn match_persisted_permission(command_line: &str, network: bool) -> Option<bool> {
    let persisted = load_persisted_file();
    match_persisted_permission_in(&persisted, command_line, network)
}

fn match_persisted_permission_in(
    persisted: &[PersistedPermissionRule],
    command_line: &str,
    network: bool,
) -> Option<bool> {
    persisted.iter().find_map(|rule| {
        if rule.network != network {
            return None;
        }
        let regex = Regex::new(&rule.regex).ok()?;
        regex.is_match(command_line).then_some(rule.allow)
    })
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

        if let Some(allow) = match_persisted_permission_in(&persisted, &command_line, false) {
            return if allow {
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
        "Glob" | "Grep" | "ListDirectory" => Some(vec![resolve_request_path(
            path_field("path").unwrap_or_else(|| request.working_dir.to_str().unwrap_or(".")),
            &request.working_dir,
        )]),
        "Git" => Some(vec![resolve_request_path(
            path_field("repo_path").unwrap_or_else(|| request.working_dir.to_str().unwrap_or(".")),
            &request.working_dir,
        )]),
        _ => None,
    }
}

fn resolve_request_path(raw_path: &str, working_dir: &Path) -> PathBuf {
    normalize_path(Path::new(raw_path), working_dir)
}

fn normalize_path(path: &Path, working_dir: &Path) -> PathBuf {
    let combined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    };

    combined.canonicalize().unwrap_or(combined)
}

fn path_within(target: &Path, allowed_root: &Path) -> bool {
    target == allowed_root || target.starts_with(allowed_root)
}

fn permission_preview(request: &PermissionRequest, command_line: &str) -> Option<String> {
    match request.tool_name.as_str() {
        "Bash" | "bash" | "Process" | "Cargo" | "Npm" | "Npx" | "PowerShell" => {
            Some(truncate_review_text(command_line))
        }
        _ if command_line != request.tool_name => Some(truncate_review_text(command_line)),
        _ => None,
    }
}

fn compact_tool_input(tool_input: &serde_json::Value) -> Option<String> {
    match tool_input {
        serde_json::Value::Null => None,
        serde_json::Value::Object(map) if map.is_empty() => None,
        _ => serde_json::to_string(tool_input).ok(),
    }
}

fn exact_command_line_regex(command_line: &str) -> String {
    format!("^{}$", regex::escape(command_line))
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

    let joined = lines.join("\n");
    let original_char_count = s.chars().count();
    let truncated_by_chars = original_char_count > MAX_REVIEW_PREVIEW_CHARS;
    let mut preview = if truncated_by_chars {
        joined
            .chars()
            .take(MAX_REVIEW_PREVIEW_CHARS)
            .collect::<String>()
    } else {
        joined
    };

    if truncated_by_lines || truncated_by_chars {
        preview.push('…');
    }

    preview
}

fn indent_review_text(s: &str) -> String {
    s.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_permission_char() -> char {
    use crossterm::event::{self, Event, KeyCode, KeyEvent};
    use crossterm::terminal;

    if terminal::enable_raw_mode().is_ok() {
        let result = loop {
            if let Ok(Event::Key(KeyEvent { code, .. })) = event::read() {
                break match code {
                    KeyCode::Char(c) => c,
                    KeyCode::Enter => 'y',
                    KeyCode::Esc => 'n',
                    _ => continue,
                };
            }
        };
        let _ = terminal::disable_raw_mode();
        eprint!("\n");
        result
    } else {
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input.trim().chars().next().unwrap_or('n')
    }
}

fn load_persisted_file_from(path: &Path) -> PersistedPermissions {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_saphyr::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_persisted_file_to(path: &Path, persisted: &PersistedPermissions) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_saphyr::to_string(persisted).unwrap_or_default());
}

fn load_persisted_file() -> PersistedPermissions {
    load_persisted_file_from(&config::permissions_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cersei_tools::PermissionLevel;
    use serde_json::json;

    fn make_request(tool_name: &str, input: serde_json::Value) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool_name.into(),
            tool_input: input,
            permission_level: PermissionLevel::Execute,
            description: "test".into(),
            id: "test-id".into(),
            working_dir: PathBuf::from("/tmp/workspace"),
        }
    }

    fn temp_permissions_path() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.yaml");
        std::mem::forget(dir);
        path
    }

    #[test]
    fn command_line_uses_literal_bash_command() {
        let req = make_request("Bash", json!({"command": "cargo build"}));
        assert_eq!(command_line_from_request(&req), "cargo build");
    }

    #[test]
    fn command_line_reconstructs_npm_command() {
        let req = make_request("Npm", json!({"args": "install react"}));
        assert_eq!(command_line_from_request(&req), "npm install react");
    }

    #[test]
    fn command_line_reconstructs_npx_command() {
        let req = make_request("Npx", json!({"args": "eslint ."}));
        assert_eq!(command_line_from_request(&req), "npx --yes eslint .");
    }

    #[test]
    fn command_line_falls_back_to_tool_and_json() {
        let req = make_request("Write", json!({"file_path": "/tmp/foo.txt"}));
        assert_eq!(
            command_line_from_request(&req),
            r#"Write {"file_path":"/tmp/foo.txt"}"#
        );
    }

    #[test]
    fn load_from_missing_file_returns_empty() {
        let path = PathBuf::from("/tmp/nonexistent_test_permissions_42.yaml");
        let loaded = load_persisted_file_from(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn yaml_round_trip_preserves_rule_order() {
        let path = temp_permissions_path();
        let perms = vec![
            PersistedPermissionRule {
                regex: "^cargo build$".into(),
                network: false,
                allow: true,
                allow_read: Vec::new(),
            },
            PersistedPermissionRule {
                regex: "^npm install react$".into(),
                network: true,
                allow: false,
                allow_read: Vec::new(),
            },
        ];

        save_persisted_file_to(&path, &perms);
        let loaded = load_persisted_file_from(&path);

        assert_eq!(loaded, perms);
    }

    #[test]
    fn malformed_yaml_returns_empty() {
        let path = temp_permissions_path();
        std::fs::write(&path, "not: [valid").unwrap();

        let loaded = load_persisted_file_from(&path);

        assert!(loaded.is_empty());
    }

    #[test]
    fn missing_network_and_allow_default_to_false() {
        let path = temp_permissions_path();
        std::fs::write(&path, "- regex: '^cargo build$'\n").unwrap();

        let loaded = load_persisted_file_from(&path);

        assert_eq!(
            loaded,
            vec![PersistedPermissionRule {
                regex: "^cargo build$".into(),
                network: false,
                allow: false,
                allow_read: Vec::new(),
            }]
        );
    }

    #[test]
    fn invalid_regex_rule_is_ignored() {
        let path = temp_permissions_path();
        let perms = vec![
            PersistedPermissionRule {
                regex: "(".into(),
                network: false,
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
        save_persisted_file_to(&path, &perms);

        let loaded = load_persisted_file_from(&path);
        let decision = loaded.into_iter().find_map(|rule| {
            if rule.network {
                return None;
            }
            let regex = Regex::new(&rule.regex).ok()?;
            regex.is_match("cargo build").then_some(rule.allow)
        });

        assert_eq!(decision, Some(false));
    }

    #[test]
    fn first_matching_rule_wins() {
        let rules = vec![
            PersistedPermissionRule {
                regex: "^cargo .*$".into(),
                network: false,
                allow: false,
                allow_read: Vec::new(),
            },
            PersistedPermissionRule {
                regex: "^cargo build$".into(),
                network: false,
                allow: true,
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
