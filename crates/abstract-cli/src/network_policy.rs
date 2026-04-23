//! Interactive network-access policy for the CLI.
//!
//! Missing `network` is treated as a normal network request so the model cannot
//! silently start commands with networking disabled. Only an explicit legacy
//! `network: "none"` bypasses the prompt and stays sandboxed. Denied network
//! requests downgrade the command to no-network mode instead of aborting a previously approved execution.
//!
//!   Network access: Npm  (requests: local network)
//!   npm install react
//!   [Y]es  [N]o  n[E]ver  [S]ession  [R]egister

use crate::permissions::{match_persisted_rule_for_request, register_command_line};
use crate::theme::Theme;
use cersei_tools::network_policy::{NetworkAccess, NetworkDecision, NetworkPolicy};
use crossterm::execute;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::Arc;

#[cfg(test)]
use crate::permissions::{match_persisted_rule_for_request_in, PersistedPermissionRule};

const MAX_REVIEW_PREVIEW_LINES: usize = 5;
const MAX_REVIEW_PREVIEW_CHARS: usize = 512;

pub struct CliNetworkPolicy {
    session_allowed: Mutex<HashSet<String>>,
    session_denied: Mutex<HashSet<String>>,
    prompt_state: Arc<PromptNetworkState>,
    theme: Theme,
}

impl CliNetworkPolicy {
    pub fn with_prompt_state(theme: &Theme, prompt_state: Arc<PromptNetworkState>) -> Self {
        Self {
            session_allowed: Mutex::new(HashSet::new()),
            session_denied: Mutex::new(HashSet::new()),
            prompt_state,
            theme: theme.clone(),
        }
    }
}

#[derive(Default)]
pub(crate) struct PromptNetworkState {
    allow_once: Mutex<HashSet<String>>,
    block_once: Mutex<HashSet<String>>,
    block_session_commands: Mutex<HashSet<String>>,
    block_session_tools: Mutex<HashSet<String>>,
}

impl PromptNetworkState {
    pub(crate) fn allow_command_once(&self, command: &str) {
        let command = command.to_string();
        self.block_once.lock().remove(&command);
        self.allow_once.lock().insert(command);
    }

    pub(crate) fn block_command_once(&self, command: &str) {
        let command = command.to_string();
        self.allow_once.lock().remove(&command);
        self.block_once.lock().insert(command);
    }

    pub(crate) fn block_command_for_session(&self, command: &str) {
        self.block_session_commands
            .lock()
            .insert(command.to_string());
    }

    pub(crate) fn block_tool_for_session(&self, tool_name: &str) {
        self.block_session_tools
            .lock()
            .insert(normalize_tool_name(tool_name));
    }

    fn take_decision(
        &self,
        tool_name: &str,
        command: &str,
        requested: NetworkAccess,
    ) -> Option<NetworkDecision> {
        if requested == NetworkAccess::Blocked {
            return Some(NetworkDecision::Allow(NetworkAccess::Blocked));
        }

        if self.allow_once.lock().remove(command) {
            return Some(NetworkDecision::Allow(requested));
        }

        if self.block_once.lock().remove(command) {
            return Some(NetworkDecision::Allow(NetworkAccess::Blocked));
        }

        if self
            .block_session_tools
            .lock()
            .contains(&normalize_tool_name(tool_name))
        {
            return Some(NetworkDecision::Allow(NetworkAccess::Blocked));
        }

        if self.block_session_commands.lock().contains(command) {
            return Some(NetworkDecision::Allow(NetworkAccess::Blocked));
        }

        None
    }
}

#[async_trait::async_trait]
impl NetworkPolicy for CliNetworkPolicy {
    async fn check(
        &self,
        tool_name: &str,
        command: &str,
        requested: NetworkAccess,
    ) -> NetworkDecision {
        if let Some(decision) = self
            .prompt_state
            .take_decision(tool_name, command, requested)
        {
            return decision;
        }

        if let Some(decision) = matched_network_decision(command, requested) {
            return decision;
        }

        if self.session_denied.lock().contains(command) {
            return NetworkDecision::Allow(NetworkAccess::Blocked);
        }
        if self.session_allowed.lock().contains(command) {
            return NetworkDecision::Allow(requested);
        }

        let preview = truncate_review_text(command);
        let access_label = match requested {
            NetworkAccess::Full => "full network",
            NetworkAccess::Local => "local network",
            NetworkAccess::Blocked => unreachable!(),
        };

        loop {
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
                Print("  [Y]es  [N]o  n[E]ver  [S]ession  [R]egister "),
                ResetColor,
            );
            let _ = io::stderr().flush();

            match read_char() {
                'y' | 'Y' | '\n' => return NetworkDecision::Allow(requested),
                's' | 'S' => {
                    self.session_allowed.lock().insert(command.to_string());
                    return NetworkDecision::Allow(requested);
                }
                'e' | 'E' => {
                    self.session_denied.lock().insert(command.to_string());
                    return NetworkDecision::Allow(NetworkAccess::Blocked);
                }
                'r' | 'R' => {
                    register_command_line(command);
                    continue;
                }
                _ => return NetworkDecision::Allow(NetworkAccess::Blocked),
            }
        }
    }
}

fn matched_network_decision(command: &str, requested: NetworkAccess) -> Option<NetworkDecision> {
    if requested == NetworkAccess::Blocked {
        return Some(NetworkDecision::Allow(NetworkAccess::Blocked));
    }

    match_persisted_rule_for_request("", command, true).map(|rule| {
        if !rule.allow {
            return NetworkDecision::Allow(NetworkAccess::Blocked);
        }

        if rule.network {
            NetworkDecision::Allow(requested)
        } else {
            NetworkDecision::Allow(NetworkAccess::Blocked)
        }
    })
}

#[cfg(test)]
fn matched_network_decision_in(
    persisted: &[PersistedPermissionRule],
    command: &str,
    requested: NetworkAccess,
) -> Option<NetworkDecision> {
    if requested == NetworkAccess::Blocked {
        return Some(NetworkDecision::Allow(NetworkAccess::Blocked));
    }

    match_persisted_rule_for_request_in(persisted, "", command, true).map(|rule| {
        if !rule.allow {
            return NetworkDecision::Allow(NetworkAccess::Blocked);
        }

        if rule.network {
            NetworkDecision::Allow(requested)
        } else {
            NetworkDecision::Allow(NetworkAccess::Blocked)
        }
    })
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
    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
    use crossterm::terminal;

    if terminal::enable_raw_mode().is_ok() {
        let result = loop {
            if let Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) = event::read()
            {
                // Ctrl-C: exit raw mode and raise SIGINT so the signal handler
                // can cancel the running agent turn.
                if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                    let _ = terminal::disable_raw_mode();
                    eprintln!();
                    unsafe { libc::raise(libc::SIGINT) };
                    return 'n';
                }
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

fn normalize_tool_name(tool_name: &str) -> String {
    tool_name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_allow_rule_grants_requested_network() {
        let rules = vec![PersistedPermissionRule {
            regex: "^cargo build$".into(),
            tool: None,
            network: true,
            allow: true,
            allow_read: Vec::new(),
        }];

        assert_eq!(
            matched_network_decision_in(&rules, "cargo build", NetworkAccess::Full),
            Some(NetworkDecision::Allow(NetworkAccess::Full))
        );
    }

    #[test]
    fn persisted_deny_rule_downgrades_to_blocked_network() {
        let rules = vec![PersistedPermissionRule {
            regex: "^cargo build$".into(),
            tool: None,
            network: true,
            allow: false,
            allow_read: Vec::new(),
        }];

        assert_eq!(
            matched_network_decision_in(&rules, "cargo build", NetworkAccess::Local),
            Some(NetworkDecision::Allow(NetworkAccess::Blocked))
        );
    }

    #[test]
    fn explicit_none_skips_network_prompting() {
        assert_eq!(
            matched_network_decision_in(&[], "cargo build", NetworkAccess::Blocked),
            Some(NetworkDecision::Allow(NetworkAccess::Blocked))
        );
    }

    #[test]
    fn non_network_rule_runs_command_without_network() {
        let rules = vec![PersistedPermissionRule {
            regex: "^cargo build$".into(),
            tool: None,
            network: false,
            allow: true,
            allow_read: Vec::new(),
        }];

        assert_eq!(
            matched_network_decision_in(&rules, "cargo build", NetworkAccess::Full),
            Some(NetworkDecision::Allow(NetworkAccess::Blocked))
        );
    }

    #[test]
    fn prompt_state_allows_network_once_before_session_block() {
        let state = PromptNetworkState::default();
        state.block_tool_for_session("Npm");
        state.allow_command_once("npm install");

        assert_eq!(
            state.take_decision("Npm", "npm install", NetworkAccess::Full),
            Some(NetworkDecision::Allow(NetworkAccess::Full))
        );
        assert_eq!(
            state.take_decision("Npm", "npm install", NetworkAccess::Full),
            Some(NetworkDecision::Allow(NetworkAccess::Blocked))
        );
    }

    #[test]
    fn prompt_state_blocks_command_for_session() {
        let state = PromptNetworkState::default();
        state.block_command_for_session("cargo test");

        assert_eq!(
            state.take_decision("Cargo", "cargo test", NetworkAccess::Local),
            Some(NetworkDecision::Allow(NetworkAccess::Blocked))
        );
    }
}
