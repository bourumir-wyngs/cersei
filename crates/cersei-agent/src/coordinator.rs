//! Coordinator mode: multi-agent orchestration.
//!
//! When active, the agent acts as a coordinator that spawns parallel worker
//! agents using the Agent tool. Workers have restricted tool access (no Agent,
//! SendMessage, TaskStop) to prevent uncontrolled recursion.

use cersei_tools::Tool;

/// Agent execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Full access to all tools including Agent spawning.
    Coordinator,
    /// Restricted: cannot spawn sub-agents or use coordination tools.
    Worker,
    /// Standard mode: all tools available (no special orchestration).
    Normal,
}

/// Tools restricted to coordinator mode only (workers can't use these).
pub const COORDINATOR_ONLY_TOOLS: &[&str] = &[
    "Agent",
    "SendMessage",
    "TaskStop",
    "TeamCreate",
    "TeamDelete",
    "SyntheticOutput",
];

/// Check if coordinator mode is active via environment variable.
pub fn is_coordinator_mode() -> bool {
    match std::env::var("CERSEI_COORDINATOR_MODE") {
        Ok(v) => !v.is_empty() && v != "0" && v != "false",
        Err(_) => false,
    }
}

/// Filter tools based on agent mode.
/// Workers lose coordinator-only tools. Coordinators and Normal keep everything.
pub fn filter_tools_for_mode(tools: Vec<Box<dyn Tool>>, mode: AgentMode) -> Vec<Box<dyn Tool>> {
    match mode {
        AgentMode::Worker => tools
            .into_iter()
            .filter(|t| !COORDINATOR_ONLY_TOOLS.contains(&t.name()))
            .collect(),
        AgentMode::Coordinator | AgentMode::Normal => tools,
    }
}

/// System prompt section for coordinator mode.
pub fn coordinator_system_prompt() -> &'static str {
    "## Coordinator Mode\n\n\
    You are operating as an orchestrator. Your role is to:\n\
    1. Break complex tasks into independent sub-tasks when delegation will help\n\
    2. Use the Agent tool for parallel workers only if it is available in this session\n\
    3. Make each worker prompt fully self-contained, including the relevant context, constraints, and expected output\n\
    4. Synthesize findings from all workers before responding or delegating follow-up work\n\
    5. Keep available task-tracking tools such as TaskCreate, TaskUpdate, or TodoWrite up to date when useful\n\n\
    Workers inherit the tools supplied by the caller, except that the Agent tool is removed to prevent recursive spawning."
}

/// Format a context section listing available tools for the coordinator.
pub fn coordinator_context(tools: &[Box<dyn Tool>]) -> String {
    let tool_list: Vec<String> = tools
        .iter()
        .filter(|t| !["Agent", "SyntheticOutput"].contains(&t.name()))
        .map(|t| format!("- {}: {}", t.name(), t.description()))
        .collect();

    format!("Available tools for workers:\n{}", tool_list.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_worker_tools() {
        let tools = cersei_tools::all();
        let original_count = tools.len();
        let filtered = filter_tools_for_mode(tools, AgentMode::Worker);
        // Workers should have fewer tools (coordinator-only removed)
        assert!(filtered.len() <= original_count);
        assert!(filtered
            .iter()
            .all(|t| !COORDINATOR_ONLY_TOOLS.contains(&t.name())));
    }

    #[test]
    fn test_filter_coordinator_keeps_all() {
        let tools = cersei_tools::all();
        let count = tools.len();
        let filtered = filter_tools_for_mode(tools, AgentMode::Coordinator);
        assert_eq!(filtered.len(), count);
    }

    #[test]
    fn test_coordinator_prompt() {
        let prompt = coordinator_system_prompt();
        assert!(prompt.contains("orchestrator"));
        assert!(prompt.contains("sub-tasks"));
        assert!(prompt.contains("only if it is available in this session"));
    }

    #[test]
    fn test_coordinator_context() {
        let tools = cersei_tools::all();
        let ctx = coordinator_context(&tools);
        assert!(ctx.contains("Available tools"));
        assert!(ctx.contains("Read"));
        assert!(ctx.contains("Bash"));
    }
}
