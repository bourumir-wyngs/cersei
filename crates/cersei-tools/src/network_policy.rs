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

// ─── Trait ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait NetworkPolicy: Send + Sync {
    /// Decide the actual access level to grant.
    ///
    /// `requested` is what the tool input declared. When `Blocked` the policy
    /// should return `Blocked` without prompting. When `Full` the policy may
    /// approve, deny, or prompt the user.
    async fn check(
        &self,
        tool_name: &str,
        command: &str,
        requested: NetworkAccess,
    ) -> NetworkAccess;
}

// ─── Built-in policies ───────────────────────────────────────────────────────

/// Grant exactly what the AI requests (no user interaction).
pub struct NetworkAllow;

#[async_trait]
impl NetworkPolicy for NetworkAllow {
    async fn check(&self, _tool: &str, _cmd: &str, requested: NetworkAccess) -> NetworkAccess {
        requested
    }
}

/// Block all network regardless of what was requested.
pub struct NetworkDeny;

#[async_trait]
impl NetworkPolicy for NetworkDeny {
    async fn check(&self, _tool: &str, _cmd: &str, _requested: NetworkAccess) -> NetworkAccess {
        NetworkAccess::Blocked
    }
}

// ─── Arc wrapper ─────────────────────────────────────────────────────────────

#[async_trait]
impl NetworkPolicy for Arc<dyn NetworkPolicy> {
    async fn check(&self, tool: &str, cmd: &str, requested: NetworkAccess) -> NetworkAccess {
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
    let net_flag = match access {
        NetworkAccess::Local => "--net=sandbox",
        _ => "--net=none",
    };
    let mut cmd = Command::new("firejail");
    cmd.args([
        "--quiet",
        "--noprofile",
        net_flag,
        "--",
        "sh",
        "-c",
        command,
    ]);
    cmd
}

#[cfg(test)]
mod tests {
    use super::NetworkAccess;

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
}
