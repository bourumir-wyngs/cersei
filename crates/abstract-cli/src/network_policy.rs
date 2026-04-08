//! Interactive network-access policy for the CLI.
//!
//! When the AI requests `network: "full"`, the user is prompted once per tool
//! type. No prompt is shown when the AI requests no network (default).
//!
//!   Network access: Npm  (requests: full network)
//!   npm install react
//!   [Y]es  [N]o  [S]ession  [A]lways
//!
//! Y / Enter = allow full network, once
//! N         = block (run sandboxed), once
//! S         = allow for the rest of the session
//! A         = always allow

use cersei_tools::network_policy::{NetworkAccess, NetworkPolicy};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::{self, Write};

pub struct CliNetworkPolicy {
    session_allowed: Mutex<HashSet<String>>,
    always_allowed: Mutex<HashSet<String>>,
}

impl CliNetworkPolicy {
    pub fn new() -> Self {
        Self {
            session_allowed: Mutex::new(HashSet::new()),
            always_allowed: Mutex::new(HashSet::new()),
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
        // AI didn't ask for network — sandbox silently, no prompt.
        if requested == NetworkAccess::Blocked {
            return NetworkAccess::Blocked;
        }

        // Check permanent then session cache.
        if self.always_allowed.lock().contains(tool_name) {
            return NetworkAccess::Full;
        }
        if self.session_allowed.lock().contains(tool_name) {
            return NetworkAccess::Full;
        }

        // Prompt user.
        let preview = if command.len() > 80 {
            format!("{}…", &command[..79])
        } else {
            command.to_string()
        };
        eprint!("\n");
        eprint!("  \x1b[33;1mNetwork access: {}\x1b[0m\n", tool_name);
        eprint!("  \x1b[90m{}\x1b[0m\n", preview);
        eprint!("  \x1b[33m[Y]es  [N]o  [S]ession  [A]lways\x1b[0m ");
        let _ = io::stderr().flush();

        match read_char() {
            'y' | 'Y' | '\n' => NetworkAccess::Full,
            's' | 'S' => {
                self.session_allowed.lock().insert(tool_name.to_string());
                NetworkAccess::Full
            }
            'a' | 'A' => {
                self.always_allowed.lock().insert(tool_name.to_string());
                NetworkAccess::Full
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
