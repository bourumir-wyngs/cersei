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
        let preview = if command.len() > 80 {
            format!("{}…", &command[..79])
        } else {
            command.to_string()
        };
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
            Print(format!("  {preview}")),
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
