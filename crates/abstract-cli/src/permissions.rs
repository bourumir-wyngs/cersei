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
    /// Permission scopes allowed for the entire session.
    session_scopes_allowed: Mutex<HashSet<SessionPermissionScope>>,
    /// Permission scopes denied for the entire session.
    session_scopes_denied: Mutex<HashSet<SessionPermissionScope>>,
    theme: Theme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SessionPermissionScope {
    WriteWorkspace,
    Pytest,
    WebTests,
    WasmTests,
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
        "Pytest" => tool_input
            .get("args")
            .and_then(|value| value.as_str())
            .map(|args| {
                if args.trim().is_empty() {
                    "pytest".to_string()
                } else {
                    format!("pytest {args}")
                }
            })
            .or_else(|| Some("pytest".to_string())),
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
            session_scopes_allowed: Mutex::new(HashSet::new()),
            session_scopes_denied: Mutex::new(HashSet::new()),
            theme: theme.clone(),
        }
    }

    fn session_scope_for_request(request: &PermissionRequest) -> Option<SessionPermissionScope> {
        match (request.permission_level, request.tool_name.as_str()) {
            (PermissionLevel::Write, _) => Some(SessionPermissionScope::WriteWorkspace),
            (PermissionLevel::Execute, "Pytest") => Some(SessionPermissionScope::Pytest),
            (PermissionLevel::Execute, "web_tests") => Some(SessionPermissionScope::WebTests),
            (PermissionLevel::Execute, "wasm_tests") => Some(SessionPermissionScope::WasmTests),
            _ => None,
        }
    }

    fn allow_scope_for_session(&self, scope: SessionPermissionScope) {
        self.session_scopes_allowed.lock().insert(scope);
    }

    fn deny_scope_for_session(&self, scope: SessionPermissionScope) {
        self.session_scopes_denied.lock().insert(scope);
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
        let session_scope = Self::session_scope_for_request(request);

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

        if let Some(scope) = session_scope {
            if self.session_scopes_denied.lock().contains(&scope) {
                return PermissionDecision::Deny(scope.session_denied_reason().into());
            }
        }

        if self.session_denied.lock().contains(&command_line) {
            return PermissionDecision::Deny("User denied (session)".into());
        }

        if let Some(scope) = session_scope {
            if self.session_scopes_allowed.lock().contains(&scope) {
                return PermissionDecision::Allow;
            }
        }

        if self.session_allowed.lock().contains(&command_line) {
            return PermissionDecision::Allow;
        }

        let permission_subject = session_scope
            .map(SessionPermissionScope::label)
            .unwrap_or_else(|| request.tool_name.as_str());
        let level_str = format!("{:?}", request.permission_level);
        let preview = permission_preview(request, &command_line);
        let scope_notice = session_scope.map(SessionPermissionScope::session_notice);

        loop {
            let _ = execute!(
                io::stderr(),
                Print("\n"),
                SetForegroundColor(self.theme.permission_accent),
                SetAttribute(crossterm::style::Attribute::Bold),
                Print(format!("  Permission required: {permission_subject}")),
                ResetColor,
                SetAttribute(crossterm::style::Attribute::Reset),
                Print("\n"),
                SetForegroundColor(self.theme.dim),
                Print(format!("  {}", request.description)),
                ResetColor,
                Print("\n"),
            );
            if let Some(scope_notice) = scope_notice {
                let _ = execute!(
                    io::stderr(),
                    SetForegroundColor(self.theme.dim),
                    Print(format!("  {scope_notice}")),
                    ResetColor,
                    Print("\n"),
                );
            }
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
                Print(permission_prompt_choices(session_scope)),
                ResetColor,
            );
            let _ = io::stderr().flush();

            match read_permission_char(valid_permission_chars(session_scope)) {
                'y' | 'Y' | '\n' => {
                    if let Some(scope) = session_scope {
                        self.allow_scope_for_session(scope);
                        return PermissionDecision::AllowForSession;
                    }
                    return PermissionDecision::AllowOnce;
                }
                's' | 'S' => {
                    if let Some(scope) = session_scope {
                        self.allow_scope_for_session(scope);
                        return PermissionDecision::AllowForSession;
                    }
                    self.session_allowed.lock().insert(command_line.clone());
                    return PermissionDecision::AllowForSession;
                }
                'e' | 'E' => {
                    if let Some(scope) = session_scope {
                        self.deny_scope_for_session(scope);
                        return PermissionDecision::Deny(scope.session_denied_reason().into());
                    }
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

impl SessionPermissionScope {
    fn label(self) -> &'static str {
        match self {
            Self::WriteWorkspace => "Write workspace",
            Self::Pytest => "Pytest",
            Self::WebTests => "Web tests",
            Self::WasmTests => "Wasm tests",
        }
    }

    fn session_notice(self) -> &'static str {
        match self {
            Self::WriteWorkspace => {
                "Granting this allows all Write-risk tools for the rest of this session."
            }
            Self::Pytest => "Granting this allows all Pytest runs for the rest of this session.",
            Self::WebTests => {
                "Granting this allows all web_tests runs for the rest of this session."
            }
            Self::WasmTests => {
                "Granting this allows all wasm_tests runs for the rest of this session."
            }
        }
    }

    fn session_denied_reason(self) -> &'static str {
        match self {
            Self::WriteWorkspace => "User denied write workspace (session)",
            Self::Pytest => "User denied pytest (session)",
            Self::WebTests => "User denied web_tests (session)",
            Self::WasmTests => "User denied wasm_tests (session)",
        }
    }
}

fn permission_prompt_choices(scope: Option<SessionPermissionScope>) -> &'static str {
    match scope {
        Some(
            SessionPermissionScope::WriteWorkspace
            | SessionPermissionScope::Pytest
            | SessionPermissionScope::WebTests
            | SessionPermissionScope::WasmTests,
        ) => "  [Y]es  [N]o  n[E]ver  Deny e[X]plaining  [R]egister ",
        None => "  [Y]es  [N]o  n[E]ver  Deny e[X]plaining  [S]ession  [R]egister ",
    }
}

fn valid_permission_chars(scope: Option<SessionPermissionScope>) -> &'static str {
    match scope {
        Some(
            SessionPermissionScope::WriteWorkspace
            | SessionPermissionScope::Pytest
            | SessionPermissionScope::WebTests
            | SessionPermissionScope::WasmTests,
        ) => "yYnNeErRxX",
        None => "yYnNeErRsExX",
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
        "File" => match path_field("action")? {
            "delete" => Some(vec![resolve_request_path(
                path_field("file_path")?,
                &request.working_dir,
            )]),
            "copy" | "move" => Some(vec![
                resolve_request_path(path_field("source_path")?, &request.working_dir),
                resolve_request_path(path_field("destination_path")?, &request.working_dir),
            ]),
            _ => None,
        },
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
        "Bash"
            | "bash"
            | "Process"
            | "PowerShell"
            | "Cargo"
            | "Npm"
            | "Npx"
            | "Pytest"
            | "web_tests"
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

fn read_permission_char(valid_chars: &str) -> char {
    use crossterm::event::{self, Event, KeyCode, KeyEvent};
    use crossterm::terminal;

    if terminal::enable_raw_mode().is_ok() {
        let result = loop {
            if let Ok(Event::Key(KeyEvent { code, .. })) = event::read() {
                break match code {
                    KeyCode::Char(c) => {
                        if valid_chars.contains(c) {
                            c
                        } else {
                            continue;
                        }
                    }
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
        // Fallback: read a line
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input
            .trim()
            .chars()
            .find(|c| valid_chars.contains(*c))
            .unwrap_or('n')
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
        make_request_with_level(tool_name, input, PermissionLevel::ReadOnly)
    }

    fn make_request_with_level(
        tool_name: &str,
        input: serde_json::Value,
        permission_level: PermissionLevel,
    ) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool_name.into(),
            tool_input: input,
            permission_level,
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
        assert_eq!(
            command_line_from_request(&make_request("Pytest", json!({ "args": "-q tests" }))),
            "pytest -q tests"
        );
        assert_eq!(
            command_line_from_request(&make_request("Pytest", json!({}))),
            "pytest"
        );
        assert_eq!(
            command_line_from_request(&make_request(
                "web_tests",
                json!({ "command": "npm run test:web" })
            )),
            "npm run test:web"
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

    #[test]
    fn write_requests_use_global_workspace_scope() {
        let request = make_request_with_level(
            "Write",
            json!({ "file_path": "/tmp/demo.txt", "content": "demo" }),
            PermissionLevel::Write,
        );

        assert_eq!(
            CliPermissionPolicy::session_scope_for_request(&request),
            Some(SessionPermissionScope::WriteWorkspace)
        );
        assert_eq!(
            CliPermissionPolicy::session_scope_for_request(&request)
                .map(SessionPermissionScope::label),
            Some("Write workspace")
        );
    }

    #[test]
    fn file_move_requests_extract_source_and_destination_targets() {
        let request = make_request_with_level(
            "File",
            json!({
                "action": "move",
                "source_path": "src/old.rs",
                "destination_path": "src/new.rs"
            }),
            PermissionLevel::Write,
        );

        let targets = extract_read_targets(&request).unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets[0].ends_with("src/old.rs"));
        assert!(targets[1].ends_with("src/new.rs"));
    }

    #[test]
    fn pytest_requests_use_pytest_session_scope() {
        let request = make_request_with_level(
            "Pytest",
            json!({ "args": "-q tests/unit" }),
            PermissionLevel::Execute,
        );

        assert_eq!(
            CliPermissionPolicy::session_scope_for_request(&request),
            Some(SessionPermissionScope::Pytest)
        );
        assert_eq!(
            CliPermissionPolicy::session_scope_for_request(&request)
                .map(SessionPermissionScope::label),
            Some("Pytest")
        );
        assert_eq!(
            permission_prompt_choices(CliPermissionPolicy::session_scope_for_request(&request)),
            "  [Y]es  [N]o  n[E]ver  Deny e[X]plaining  [R]egister "
        );
    }

    #[test]
    fn web_tests_requests_use_web_tests_session_scope() {
        let request = make_request_with_level(
            "web_tests",
            json!({ "command": "npm run test:web" }),
            PermissionLevel::Execute,
        );

        assert_eq!(
            CliPermissionPolicy::session_scope_for_request(&request),
            Some(SessionPermissionScope::WebTests)
        );
        assert_eq!(
            CliPermissionPolicy::session_scope_for_request(&request)
                .map(SessionPermissionScope::label),
            Some("Web tests")
        );
        assert_eq!(
            permission_prompt_choices(CliPermissionPolicy::session_scope_for_request(&request)),
            "  [Y]es  [N]o  n[E]ver  Deny e[X]plaining  [R]egister "
        );
    }

    #[tokio::test]
    async fn session_write_scope_allows_other_write_tools() {
        let policy = CliPermissionPolicy::new(&Theme::dark());
        policy.allow_scope_for_session(SessionPermissionScope::WriteWorkspace);

        let write_request = make_request_with_level(
            "Write",
            json!({ "file_path": "/tmp/a.txt", "content": "hello" }),
            PermissionLevel::Write,
        );
        let notebook_request = make_request_with_level(
            "NotebookEdit",
            json!({ "file_path": "/tmp/demo.ipynb", "cell_index": 0, "new_source": "print(1)" }),
            PermissionLevel::Write,
        );

        assert!(matches!(
            policy.check(&write_request).await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            policy.check(&notebook_request).await,
            PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn session_pytest_scope_allows_other_pytest_commands() {
        let policy = CliPermissionPolicy::new(&Theme::dark());
        policy.allow_scope_for_session(SessionPermissionScope::Pytest);

        let unit_request = make_request_with_level(
            "Pytest",
            json!({ "args": "-q tests/unit" }),
            PermissionLevel::Execute,
        );
        let integration_request = make_request_with_level(
            "Pytest",
            json!({ "args": "-q tests/integration -k parser" }),
            PermissionLevel::Execute,
        );

        assert!(matches!(
            policy.check(&unit_request).await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            policy.check(&integration_request).await,
            PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn session_web_tests_scope_allows_other_web_tests_commands() {
        let policy = CliPermissionPolicy::new(&Theme::dark());
        policy.allow_scope_for_session(SessionPermissionScope::WebTests);

        let test_request = make_request_with_level(
            "web_tests",
            json!({ "command": "npm run test:frontend" }),
            PermissionLevel::Execute,
        );
        let lint_request = make_request_with_level(
            "web_tests",
            json!({ "command": "npx --yes eslint src/app.tsx" }),
            PermissionLevel::Execute,
        );

        assert!(matches!(
            policy.check(&test_request).await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            policy.check(&lint_request).await,
            PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn session_write_scope_does_not_affect_execute_requests() {
        let policy = CliPermissionPolicy::new(&Theme::dark());
        policy.allow_scope_for_session(SessionPermissionScope::WriteWorkspace);

        let request = make_request_with_level(
            "Bash",
            json!({ "command": "echo hi" }),
            PermissionLevel::Execute,
        );

        assert!(!matches!(
            CliPermissionPolicy::session_scope_for_request(&request),
            Some(SessionPermissionScope::WriteWorkspace)
        ));
        assert_eq!(
            permission_prompt_choices(CliPermissionPolicy::session_scope_for_request(&request)),
            "  [Y]es  [N]o  n[E]ver  Deny e[X]plaining  [S]ession  [R]egister "
        );
    }

    #[tokio::test]
    async fn session_pytest_scope_does_not_affect_other_execute_tools() {
        let policy = CliPermissionPolicy::new(&Theme::dark());
        policy.allow_scope_for_session(SessionPermissionScope::Pytest);

        let request = make_request_with_level(
            "Bash",
            json!({ "command": "echo hi" }),
            PermissionLevel::Execute,
        );

        assert_eq!(
            CliPermissionPolicy::session_scope_for_request(&request),
            None
        );
        assert_eq!(
            permission_prompt_choices(CliPermissionPolicy::session_scope_for_request(&request)),
            "  [Y]es  [N]o  n[E]ver  Deny e[X]plaining  [S]ession  [R]egister "
        );
    }
}
