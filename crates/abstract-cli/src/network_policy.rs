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
        eprint!("\n");
        eprint!("  \x1b[33;1mNetwork access: {}  ({})\x1b[0m\n", tool_name, access_label);
        eprint!("  \x1b[90m{}\x1b[0m\n", preview);
        eprint!("  \x1b[33m[Y]es  [N]o  [S]ession  [A]lways\x1b[0m ");
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
