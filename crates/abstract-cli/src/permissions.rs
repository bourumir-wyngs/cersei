//! Interactive permission UI for the CLI.
//!
//! Implements PermissionPolicy by prompting the user in the terminal.
//! Caches session-level and permanent allow decisions.
//! Permanent ("always") decisions are persisted to ~/.abstract/permissions.json.

use crate::config;
use crate::theme::Theme;
use cersei_tools::permissions::{PermissionDecision, PermissionPolicy, PermissionRequest};
use cersei_tools::PermissionLevel;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetAttribute, SetForegroundColor};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::{self, Write};

const MAX_REVIEW_PREVIEW_LINES: usize = 5;
const MAX_REVIEW_PREVIEW_CHARS: usize = 512;

/// Interactive permission policy for the CLI.
/// Prompts user for Write/Execute/Dangerous tools, auto-allows ReadOnly/None.
pub struct CliPermissionPolicy {
    /// Tools allowed for the entire session (by composite key).
    session_allowed: Mutex<HashSet<String>>,
    /// Tools permanently allowed (by composite key), persisted to disk.
    always_allowed: Mutex<HashSet<String>>,
    theme: Theme,
}

/// Build a composite permission key from a request.
/// For tools with a `command` field (Bash, Process), the key includes the command
/// so that "always allow" is scoped to the specific command, not the tool globally.
fn permission_key(request: &PermissionRequest) -> String {
    let command = request
        .tool_input
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if command.is_empty() {
        request.tool_name.clone()
    } else {
        format!("{}:{}", request.tool_name, command)
    }
}

impl CliPermissionPolicy {
    pub fn new(theme: &Theme) -> Self {
        let always_allowed = load_persisted_permissions();
        Self {
            session_allowed: Mutex::new(HashSet::new()),
            always_allowed: Mutex::new(always_allowed),
            theme: theme.clone(),
        }
    }
}

#[async_trait::async_trait]
impl PermissionPolicy for CliPermissionPolicy {
    async fn check(&self, request: &PermissionRequest) -> PermissionDecision {
        // Auto-allow safe operations
        match request.permission_level {
            PermissionLevel::None | PermissionLevel::ReadOnly => {
                return PermissionDecision::Allow;
            }
            PermissionLevel::Forbidden => {
                return PermissionDecision::Deny("Operation is forbidden".into());
            }
            _ => {}
        }

        let key = permission_key(request);

        // Check caches
        if self.always_allowed.lock().contains(&key) {
            return PermissionDecision::Allow;
        }
        if self.session_allowed.lock().contains(&key) {
            return PermissionDecision::Allow;
        }

        // Prompt user
        let level_str = format!("{:?}", request.permission_level);
        let preview = permission_preview(request);
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
        if let Some(preview) = preview {
            let _ = execute!(
                io::stderr(),
                SetForegroundColor(self.theme.review_text),
                Print(indent_review_text(&preview)),
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
            Print("  [Y]es  [N]o  [S]ession  [A]lways "),
            ResetColor,
        );
        let _ = io::stderr().flush();

        let decision = read_permission_char();

        match decision {
            'y' | 'Y' | '\n' => PermissionDecision::AllowOnce,
            's' | 'S' => {
                self.session_allowed.lock().insert(key);
                PermissionDecision::AllowForSession
            }
            'a' | 'A' => {
                self.always_allowed.lock().insert(key);
                save_persisted_permissions(&self.always_allowed.lock());
                PermissionDecision::Allow
            }
            _ => PermissionDecision::Deny("User denied".into()),
        }
    }
}

fn permission_preview(request: &PermissionRequest) -> Option<String> {
    match request.tool_name.as_str() {
        "Bash" | "bash" => request
            .tool_input
            .get("command")
            .and_then(|v| v.as_str())
            .map(truncate_review_text),
        "Process" => request
            .tool_input
            .get("command")
            .and_then(|v| v.as_str())
            .map(truncate_review_text),
        _ => None,
    }
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
    // Try to read a single character
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
        // Fallback: read a line
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input.trim().chars().next().unwrap_or('n')
    }
}

// ─── Persistence ──────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Default, Debug, PartialEq)]
struct PersistedPermissions {
    #[serde(default)]
    tool_permissions: Vec<String>,
    #[serde(default)]
    network_permissions: Vec<String>,
}

fn load_persisted_file_from(path: &std::path::Path) -> PersistedPermissions {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_persisted_file_to(path: &std::path::Path, persisted: &PersistedPermissions) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(persisted).unwrap_or_default(),
    );
}

fn load_persisted_file() -> PersistedPermissions {
    load_persisted_file_from(&config::permissions_path())
}

fn load_persisted_permissions() -> HashSet<String> {
    load_persisted_file().tool_permissions.into_iter().collect()
}

fn save_persisted_permissions(allowed: &HashSet<String>) {
    let path = config::permissions_path();
    let mut persisted = load_persisted_file_from(&path);
    persisted.tool_permissions = allowed.iter().cloned().collect();
    persisted.tool_permissions.sort();
    save_persisted_file_to(&path, &persisted);
}

pub fn load_persisted_network_permissions() -> HashSet<String> {
    load_persisted_file()
        .network_permissions
        .into_iter()
        .collect()
}

pub fn save_persisted_network_permissions(allowed: &HashSet<String>) {
    let path = config::permissions_path();
    let mut persisted = load_persisted_file_from(&path);
    persisted.network_permissions = allowed.iter().cloned().collect();
    persisted.network_permissions.sort();
    save_persisted_file_to(&path, &persisted);
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cersei_tools::PermissionLevel;
    use serde_json::json;
    use std::path::PathBuf;

    fn make_request(tool_name: &str, input: serde_json::Value) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool_name.into(),
            tool_input: input,
            permission_level: PermissionLevel::Execute,
            description: "test".into(),
            id: "test-id".into(),
        }
    }

    // ── permission_key tests ──────────────────────────────────────────────

    #[test]
    fn key_includes_command_for_bash() {
        let req = make_request("Bash", json!({"command": "cargo build"}));
        assert_eq!(permission_key(&req), "Bash:cargo build");
    }

    #[test]
    fn key_includes_command_for_process() {
        let req = make_request("Process", json!({"command": "npm start"}));
        assert_eq!(permission_key(&req), "Process:npm start");
    }

    #[test]
    fn key_is_tool_name_only_when_no_command() {
        let req = make_request("Write", json!({"file_path": "/tmp/foo.txt"}));
        assert_eq!(permission_key(&req), "Write");
    }

    #[test]
    fn key_is_tool_name_only_for_empty_command() {
        let req = make_request("Bash", json!({"command": ""}));
        assert_eq!(permission_key(&req), "Bash");
    }

    #[test]
    fn different_commands_produce_different_keys() {
        let a = make_request("Bash", json!({"command": "cargo build"}));
        let b = make_request("Bash", json!({"command": "rm -rf /"}));
        assert_ne!(permission_key(&a), permission_key(&b));
    }

    // ── persistence round-trip tests ──────────────────────────────────────

    fn temp_permissions_path() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so it stays alive for the test
        let path = dir.path().join("permissions.json");
        std::mem::forget(dir);
        path
    }

    #[test]
    fn load_from_missing_file_returns_empty() {
        let path = PathBuf::from("/tmp/nonexistent_test_permissions_42.json");
        let loaded = load_persisted_file_from(&path);
        assert_eq!(loaded, PersistedPermissions::default());
    }

    #[test]
    fn round_trip_tool_permissions() {
        let path = temp_permissions_path();
        let mut perms = PersistedPermissions::default();
        perms.tool_permissions = vec!["Bash:cargo build".into(), "Process:npm test".into()];
        save_persisted_file_to(&path, &perms);

        let loaded = load_persisted_file_from(&path);
        assert_eq!(loaded.tool_permissions, perms.tool_permissions);
        assert!(loaded.network_permissions.is_empty());
    }

    #[test]
    fn round_trip_network_permissions() {
        let path = temp_permissions_path();
        let mut perms = PersistedPermissions::default();
        perms.network_permissions = vec!["Npm:npm install react".into()];
        save_persisted_file_to(&path, &perms);

        let loaded = load_persisted_file_from(&path);
        assert_eq!(loaded.network_permissions, perms.network_permissions);
        assert!(loaded.tool_permissions.is_empty());
    }

    #[test]
    fn saving_tool_permissions_preserves_network_permissions() {
        let path = temp_permissions_path();

        // First save network permissions
        let mut perms = PersistedPermissions::default();
        perms.network_permissions = vec!["Npm:npm install".into()];
        save_persisted_file_to(&path, &perms);

        // Now simulate saving tool permissions (read-modify-write)
        let mut loaded = load_persisted_file_from(&path);
        loaded.tool_permissions = vec!["Bash:cargo build".into()];
        loaded.tool_permissions.sort();
        save_persisted_file_to(&path, &loaded);

        // Both should be present
        let final_state = load_persisted_file_from(&path);
        assert_eq!(final_state.tool_permissions, vec!["Bash:cargo build"]);
        assert_eq!(final_state.network_permissions, vec!["Npm:npm install"]);
    }

    #[test]
    fn saved_permissions_are_sorted() {
        let path = temp_permissions_path();
        let mut perms = PersistedPermissions::default();
        perms.tool_permissions = vec!["Process:z".into(), "Bash:a".into(), "Bash:m".into()];
        perms.tool_permissions.sort();
        save_persisted_file_to(&path, &perms);

        let loaded = load_persisted_file_from(&path);
        assert_eq!(
            loaded.tool_permissions,
            vec!["Bash:a", "Bash:m", "Process:z"]
        );
    }

    #[test]
    fn malformed_json_returns_empty() {
        let path = temp_permissions_path();
        std::fs::write(&path, "not valid json {{{").unwrap();
        let loaded = load_persisted_file_from(&path);
        assert_eq!(loaded, PersistedPermissions::default());
    }

    #[test]
    fn partial_json_loads_with_defaults() {
        let path = temp_permissions_path();
        std::fs::write(&path, r#"{"tool_permissions": ["Bash:ls"]}"#).unwrap();
        let loaded = load_persisted_file_from(&path);
        assert_eq!(loaded.tool_permissions, vec!["Bash:ls"]);
        assert!(loaded.network_permissions.is_empty());
    }
}
