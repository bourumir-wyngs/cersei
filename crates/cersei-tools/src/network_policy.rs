//! Network policy for sandboxing outbound network access in shell tool execution.
//!
//! The AI may declare network access via a `network` field in tool input:
//! - omitted / `"full"` (default) — request full network access
//! - `"local"` — local network only; run under `firejail --net=sandbox`
//! - `"none"` — legacy explicit opt-out; run under `firejail --net=none`
//!
//! ## Sandbox backend
//!
//! Uses `firejail --quiet --noprofile` when available (probed once at startup).
//! Falls back to unsandboxed execution if firejail is not installed, with a
//! one-time warning via [`sandbox_warning`].

use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::ffi::OsString;
use std::sync::Arc;
use tokio::process::Command;

// ─── Sandbox availability probe ───────────────────────────────────────────────

static FIREJAIL_AVAILABLE: Lazy<bool> = Lazy::new(|| {
    std::process::Command::new("firejail")
        .args(["--quiet", "--noprofile", "--net=none", "--", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
});

/// Returns `true` if firejail network sandboxing is available in this environment.
pub fn sandbox_available() -> bool {
    *FIREJAIL_AVAILABLE
}

/// Returns a warning string when sandboxing is unavailable, `None` otherwise.
pub fn sandbox_warning() -> Option<&'static str> {
    if !*FIREJAIL_AVAILABLE {
        Some(
            "Network sandboxing unavailable (firejail not found or not functional). \
              Commands run with full network access.",
        )
    } else {
        None
    }
}

// ─── Access level ─────────────────────────────────────────────────────────────

/// Whether a tool invocation may access the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAccess {
    /// Full unrestricted network access.
    Full,
    /// Local network only — sandboxed with `firejail --net=sandbox`.
    Local,
    /// No network — sandboxed with `firejail --net=none` (or plain `sh` if unavailable).
    Blocked,
}

impl NetworkAccess {
    /// Parse from the tool input field.
    ///
    /// Missing `network` now defaults to `Full` so the model cannot silently
    /// opt out of network permission prompts by omission. `"none"` remains
    /// accepted for backwards compatibility with existing direct callers.
    pub fn from_input(s: Option<&str>) -> Self {
        match s {
            Some("local") => Self::Local,
            Some("none") => Self::Blocked,
            Some("full") | None => Self::Full,
            Some(_) => Self::Blocked,
        }
    }
}

/// Result of evaluating a network request for a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkDecision {
    Allow(NetworkAccess),
    Deny(String),
}

// ─── Trait ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait NetworkPolicy: Send + Sync {
    /// Decide whether the command may run and, if so, what access level it gets.
    ///
    /// `requested` is what the tool input declared. When `Blocked` the policy
    /// should allow the command to proceed without prompting. When a command
    /// requests network access, the policy may approve, deny, or prompt.
    async fn check(
        &self,
        tool_name: &str,
        command: &str,
        requested: NetworkAccess,
    ) -> NetworkDecision;
}

// ─── Built-in policies ───────────────────────────────────────────────────────

/// Grant exactly what the AI requests (no user interaction).
pub struct NetworkAllow;

#[async_trait]
impl NetworkPolicy for NetworkAllow {
    async fn check(&self, _tool: &str, _cmd: &str, requested: NetworkAccess) -> NetworkDecision {
        NetworkDecision::Allow(requested)
    }
}

/// Deny commands that request network access.
pub struct NetworkDeny;

#[async_trait]
impl NetworkPolicy for NetworkDeny {
    async fn check(&self, _tool: &str, _cmd: &str, requested: NetworkAccess) -> NetworkDecision {
        if requested == NetworkAccess::Blocked {
            NetworkDecision::Allow(NetworkAccess::Blocked)
        } else {
            NetworkDecision::Deny("Network access denied by policy".into())
        }
    }
}

// ─── Arc wrapper ─────────────────────────────────────────────────────────────

#[async_trait]
impl NetworkPolicy for Arc<dyn NetworkPolicy> {
    async fn check(&self, tool: &str, cmd: &str, requested: NetworkAccess) -> NetworkDecision {
        self.as_ref().check(tool, cmd, requested).await
    }
}

// ─── Command builder ─────────────────────────────────────────────────────────

/// Build a shell command with the appropriate network sandbox applied.
///
/// - `Full`    → `sh -c <command>`
/// - `Local`   → `firejail --quiet --noprofile --net=sandbox -- sh -c <command>`
///               (falls back to `sh -c` if firejail is unavailable)
/// - `Blocked` → `firejail --quiet --noprofile --net=none -- sh -c <command>`
///               (falls back to `sh -c` if firejail is unavailable)
pub fn shell_command(command: &str, access: NetworkAccess) -> Command {
    if access == NetworkAccess::Full || !*FIREJAIL_AVAILABLE {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        return cmd;
    }

    firejail_shell_command(command, access, &[])
}

/// Build a shell command under Firejail when available, plus additional
/// Firejail arguments for filesystem confinement or other restrictions.
///
/// Unlike [`shell_command`], `NetworkAccess::Full` still runs inside Firejail;
/// it simply omits any `--net=` restriction.
pub fn firejailed_shell_command_with_extra_firejail_args(
    command: &str,
    access: NetworkAccess,
    extra_firejail_args: &[OsString],
) -> Command {
    if !*FIREJAIL_AVAILABLE {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        return cmd;
    }

    firejail_shell_command(command, access, extra_firejail_args)
}

fn firejail_shell_command(
    command: &str,
    access: NetworkAccess,
    extra_firejail_args: &[OsString],
) -> Command {
    let mut cmd = Command::new("firejail");
    cmd.args(["--quiet", "--noprofile"]);

    match access {
        NetworkAccess::Full => {}
        NetworkAccess::Local => {
            cmd.arg("--net=sandbox");
        }
        NetworkAccess::Blocked => {
            cmd.arg("--net=none");
        }
    }

    cmd.args(extra_firejail_args);
    cmd.args(["--", "sh", "-c", command]);
    cmd
}

#[cfg(test)]
mod tests {
    use super::{NetworkAccess, NetworkAllow, NetworkDecision, NetworkDeny, NetworkPolicy};

    #[test]
    fn missing_network_defaults_to_full() {
        assert_eq!(NetworkAccess::from_input(None), NetworkAccess::Full);
    }

    #[test]
    fn local_and_full_are_preserved() {
        assert_eq!(
            NetworkAccess::from_input(Some("local")),
            NetworkAccess::Local
        );
        assert_eq!(NetworkAccess::from_input(Some("full")), NetworkAccess::Full);
    }

    #[test]
    fn legacy_none_still_blocks() {
        assert_eq!(
            NetworkAccess::from_input(Some("none")),
            NetworkAccess::Blocked
        );
    }

    #[tokio::test]
    async fn deny_policy_blocks_networked_requests() {
        let policy = NetworkDeny;

        assert_eq!(
            policy
                .check("Bash", "curl https://example.com", NetworkAccess::Full)
                .await,
            NetworkDecision::Deny("Network access denied by policy".into())
        );
    }

    #[tokio::test]
    async fn deny_policy_allows_explicitly_blocked_requests() {
        let policy = NetworkDeny;

        assert_eq!(
            policy
                .check("Bash", "echo hello", NetworkAccess::Blocked)
                .await,
            NetworkDecision::Allow(NetworkAccess::Blocked)
        );
    }

    #[tokio::test]
    async fn allow_policy_returns_requested_access() {
        let policy = NetworkAllow;

        assert_eq!(
            policy
                .check("Cargo", "cargo build", NetworkAccess::Local)
                .await,
            NetworkDecision::Allow(NetworkAccess::Local)
        );
    }
}
