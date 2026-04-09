//! Interactive permission UI for the CLI.
//!
//! Implements PermissionPolicy by prompting the user in the terminal.
//! Caches session-level allow decisions.

use crate::theme::Theme;
use cersei_tools::permissions::{PermissionDecision, PermissionPolicy, PermissionRequest};
use cersei_tools::PermissionLevel;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetAttribute, SetForegroundColor};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::{self, Write};

/// Interactive permission policy for the CLI.
/// Prompts user for Write/Execute/Dangerous tools, auto-allows ReadOnly/None.
pub struct CliPermissionPolicy {
    /// Tools allowed for the entire session (by tool name).
    session_allowed: Mutex<HashSet<String>>,
    /// Tools permanently allowed (by tool name).
    always_allowed: Mutex<HashSet<String>>,
    theme: Theme,
}

impl CliPermissionPolicy {
    pub fn new(theme: &Theme) -> Self {
        Self {
            session_allowed: Mutex::new(HashSet::new()),
            always_allowed: Mutex::new(HashSet::new()),
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

        // Check caches
        if self.always_allowed.lock().contains(&request.tool_name) {
            return PermissionDecision::Allow;
        }
        if self.session_allowed.lock().contains(&request.tool_name) {
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
                Print(format!("  {preview}")),
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
                self.session_allowed
                    .lock()
                    .insert(request.tool_name.clone());
                PermissionDecision::AllowForSession
            }
            'a' | 'A' => {
                self.always_allowed.lock().insert(request.tool_name.clone());
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
            .map(|s| truncate(s, 120)),
        "Process" => request
            .tool_input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| truncate(s, 120)),
        _ => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
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
