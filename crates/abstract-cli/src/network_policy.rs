//! Interactive network-access policy for the CLI.
//!
//! Missing `network` is treated as a normal network request so the model cannot
//! silently start commands with networking disabled. Only an explicit legacy
//! `network: "none"` bypasses the prompt and stays sandboxed.
//!
//!   Network access: Npm  (requests: local network)
//!   npm install react
//!   [Y]es  [N]o  [S]ession  [A]lways
//!
//! Y / Enter = allow the requested access level, once
//! N         = block (run sandboxed with --net=none), once
//! S         = allow for the rest of the session
//! A         = always allow

use crate::theme::Theme;
use cersei_tools::network_policy::{NetworkAccess, NetworkPolicy};
use crossterm::execute;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::{self, Write};

const MAX_REVIEW_PREVIEW_LINES: usize = 5;
const MAX_REVIEW_PREVIEW_CHARS: usize = 512;

pub struct CliNetworkPolicy {
    session_allowed: Mutex<HashSet<String>>,
    always_allowed: Mutex<HashSet<String>>,
    theme: Theme,
}

impl CliNetworkPolicy {
    pub fn new(theme: &Theme) -> Self {
        Self {
            session_allowed: Mutex::new(HashSet::new()),
            always_allowed: Mutex::new(HashSet::new()),
            theme: theme.clone(),
        }
    }
}

#[async_trait::async_trait]
impl NetworkPolicy for CliNetworkPolicy {
    async fn check(
        &self,
        tool_name: &str,
        command: &str,
        requested: NetworkAccess,
    ) -> NetworkAccess {
        // Explicit no-network request — honor it without prompting.
        if requested == NetworkAccess::Blocked {
            return NetworkAccess::Blocked;
        }

        // Check permanent then session cache.
        if self.always_allowed.lock().contains(tool_name) {
            return requested;
        }
        if self.session_allowed.lock().contains(tool_name) {
            return requested;
        }

        // Prompt user.
        let preview = truncate_review_text(command);
        let access_label = match requested {
            NetworkAccess::Full => "full network",
            NetworkAccess::Local => "local network",
            NetworkAccess::Blocked => unreachable!(),
        };
        let _ = execute!(
            io::stderr(),
            Print("\n"),
            SetForegroundColor(self.theme.permission_accent),
            SetAttribute(Attribute::Bold),
            Print(format!(
                "  Network access: {}  ({})",
                tool_name, access_label
            )),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Print("\n"),
            SetForegroundColor(self.theme.review_text),
            Print(indent_review_text(&preview)),
            ResetColor,
            Print("\n"),
            SetForegroundColor(self.theme.permission_accent),
            Print("  [Y]es  [N]o  [S]ession  [A]lways "),
            ResetColor,
        );
        let _ = io::stderr().flush();

        match read_char() {
            'y' | 'Y' | '\n' => requested,
            's' | 'S' => {
                self.session_allowed.lock().insert(tool_name.to_string());
                requested
            }
            'a' | 'A' => {
                self.always_allowed.lock().insert(tool_name.to_string());
                requested
            }
            _ => NetworkAccess::Blocked,
        }
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

fn read_char() -> char {
    use crossterm::event::{self, Event, KeyCode, KeyEvent};
    use crossterm::terminal;

    if terminal::enable_raw_mode().is_ok() {
        let result = loop {
            if let Ok(Event::Key(KeyEvent { code, .. })) = event::read() {
                break match code {
                    KeyCode::Char(c) => c,
                    KeyCode::Enter => '\n',
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
